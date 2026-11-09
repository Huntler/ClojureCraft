//! Applies a fix given as whole top-level forms to the code it fixes.
//!
//! In a conversation about failing code the model answers with only the forms it changes or
//! adds, each complete: `(defn- parse-weight …)`. A form replaces the form of the same name, and
//! a new name is inserted before the first form that uses it, so it is defined before use. An
//! `ns` form replaces the namespace declaration; an answer with the `ns` form and the public
//! function is a complete rewrite and replaces everything. Matching whole forms by name cannot
//! fail on whitespace, unlike text diffs, which small models get wrong.

use crate::structure::{Form, read_all};
use clojurecraft_edn::Delim;

/// The merged code and what changed, for the attempt's notes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub code: String,
    pub replaced: Vec<String>,
    pub added: Vec<String>,
    /// The answer was a complete namespace that replaced the code.
    pub rewritten: bool,
}

impl Applied {
    /// A one-line description of the change.
    pub fn summary(&self) -> String {
        if self.rewritten {
            return "The model rewrote the whole namespace.".into();
        }
        let mut parts = Vec::new();
        if !self.replaced.is_empty() {
            parts.push(format!("changed {}", self.replaced.join(", ")));
        }
        if !self.added.is_empty() {
            parts.push(format!("added {}", self.added.join(", ")));
        }
        if parts.is_empty() {
            "The model changed nothing.".into()
        } else {
            format!("The model {}.", parts.join(" and "))
        }
    }
}

/// A top-level form's head and name, if it defines one, and where it is.
type Named<'a> = (Option<(&'a str, &'a str)>, std::ops::Range<usize>);

const DEFINERS: [&str; 5] = ["defn", "defn-", "def", "declare", "ns"];

/// The head (`defn`, `ns`, …) and name of a top-level form.
fn head_and_name(f: &Form) -> Option<(&str, &str)> {
    let Form::Seq {
        delim: Delim::Paren,
        items,
        ..
    } = f.bare()
    else {
        return None;
    };
    let head = items.first()?.symbol()?;
    DEFINERS.contains(&head).then_some(())?;
    Some((head, items.get(1)?.symbol()?))
}

/// Whether `text` uses `name` as a whole symbol.
fn uses(text: &str, name: &str) -> bool {
    let boundary = |c: char| c.is_whitespace() || "()[]{}\"',`@^#~".contains(c);
    text.match_indices(name).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + name.len()..].chars().next();
        before.is_none_or(boundary) && after.is_none_or(boundary)
    })
}

/// Applies `edit` (complete top-level forms) to `base`; `function` is the public function.
pub fn apply(base: &str, edit: &str, function: &str) -> Result<Applied, String> {
    let edit_forms = read_all(edit).map_err(|(m, _)| format!("the changed forms cannot be read: {m}"))?;
    if edit_forms.is_empty() {
        return Err("the answer changes no form".into());
    }
    let named: Vec<(&str, &str, &str)> = edit_forms
        .iter()
        .map(|f| {
            head_and_name(f)
                .map(|(h, n)| (h, n, &edit[f.span()]))
                .ok_or_else(|| {
                    "every changed form must be a complete ns, defn, defn-, def, or declare form".to_owned()
                })
        })
        .collect::<Result<_, _>>()?;
    let has_ns = named.iter().any(|(h, ..)| *h == "ns");
    let has_function = named.iter().any(|(h, n, _)| *h != "ns" && *n == function);
    let complete = || Applied {
        code: format!("{}\n", edit.trim()),
        replaced: Vec::new(),
        added: Vec::new(),
        rewritten: true,
    };
    if has_ns && has_function {
        return Ok(complete());
    }
    let Ok(base_forms) = read_all(base) else {
        return if has_ns {
            Ok(complete())
        } else {
            Err("the previous code cannot be read, so the answer must be the complete namespace".into())
        };
    };
    let base_named: Vec<Named<'_>> = base_forms.iter().map(|f| (head_and_name(f), f.span())).collect();
    // Replacements and insertions as (position in base, removed length, new text).
    let mut changes: Vec<(usize, usize, String)> = Vec::new();
    let mut applied = Applied {
        code: String::new(),
        replaced: Vec::new(),
        added: Vec::new(),
        rewritten: false,
    };
    for (i, (head, name, text)) in named.iter().enumerate() {
        // A later form of the same name wins.
        if named[i + 1..]
            .iter()
            .any(|(h, n, _)| (*h == "ns") == (*head == "ns") && n == name)
        {
            continue;
        }
        let existing = base_named.iter().find(|(hn, _)| {
            hn.is_some_and(|(h, n)| {
                if *head == "ns" {
                    h == "ns"
                } else {
                    h != "ns" && h != "declare" && n == *name
                }
            })
        });
        if let Some((_, span)) = existing {
            if &base[span.clone()] != *text {
                changes.push((span.start, span.len(), (*text).to_owned()));
                applied.replaced.push((*name).to_owned());
            }
            continue;
        }
        if *head == "declare" {
            continue;
        }
        // The text each form will have, so a use in a changed form counts.
        let effective = |hn: &Option<(&str, &str)>, span: &std::ops::Range<usize>| -> String {
            hn.and_then(|(h, n)| {
                named
                    .iter()
                    .rev()
                    .find(|(eh, en, _)| *eh != "ns" && *eh != "declare" && h != "ns" && *en == n)
                    .map(|(.., t)| (*t).to_owned())
            })
            .unwrap_or_else(|| base[span.clone()].to_owned())
        };
        let at = base_named
            .iter()
            .skip(1)
            .find(|(hn, span)| uses(&effective(hn, span), name))
            .or_else(|| {
                base_named
                    .iter()
                    .find(|(hn, _)| hn.is_some_and(|(h, n)| h != "ns" && n == function))
            })
            .map_or(base.trim_end().len(), |(_, span)| span.start);
        let insert = if at >= base.trim_end().len() {
            format!("\n\n{text}")
        } else {
            format!("{text}\n\n")
        };
        changes.push((at, 0, insert));
        applied.added.push((*name).to_owned());
    }
    // Apply from the end so earlier positions stay valid; insertions before replacements at
    // the same position.
    changes.sort_by_key(|(at, len, _)| (std::cmp::Reverse(*at), std::cmp::Reverse(*len)));
    let mut code = base.to_owned();
    for (at, len, text) in changes {
        code.replace_range(at..at + len, &text);
    }
    applied.code = format!("{}\n", code.trim_end());
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "(ns w.core\n  (:require [clojure.string :as str]))\n\n(defn- parse-number [s]\n  (parse-long s))\n\n(defn- entry [line]\n  (let [[d w & more] (str/split line #\"\\s+\")]\n    {:date d :weight (parse-number (last more))}))\n\n(defn w [input]\n  (mapv entry (str/split-lines input)))\n";

    #[test]
    fn a_changed_form_replaces_the_form_of_the_same_name() {
        let a = apply(
            BASE,
            "(defn- parse-number [s]\n  (parse-long (str/replace s #\"kg$\" \"\")))",
            "w",
        )
        .unwrap();
        assert!(
            a.code.contains("(parse-long (str/replace s #\"kg$\" \"\"))"),
            "{}",
            a.code
        );
        assert!(!a.code.contains("(parse-long s))"), "{}", a.code);
        assert!(a.code.contains("(defn- entry [line]"));
        assert_eq!(a.replaced, ["parse-number"]);
        assert_eq!(a.summary(), "The model changed parse-number.");
    }

    #[test]
    fn a_new_helper_is_defined_before_its_first_use() {
        let edit = "(defn- parse-weight [t]\n  (parse-double (str/replace t #\"kg$\" \"\")))\n\n(defn- entry [line]\n  (let [[d w & more] (str/split line #\"\\s+\")]\n    {:date d :weight (parse-weight (last more))}))";
        let a = apply(BASE, edit, "w").unwrap();
        let weight = a.code.find("(defn- parse-weight").unwrap();
        let entry = a.code.find("(defn- entry").unwrap();
        assert!(weight < entry, "{}", a.code);
        assert_eq!(a.code.matches("(defn- entry").count(), 1);
        assert_eq!(
            (a.added.as_slice(), a.replaced.as_slice()),
            (&["parse-weight".to_owned()][..], &["entry".to_owned()][..])
        );
        assert!(read_all(&a.code).is_ok());
    }

    #[test]
    fn a_complete_namespace_replaces_everything() {
        let edit = "(ns w.core)\n\n(defn w [input] [])";
        let a = apply(BASE, edit, "w").unwrap();
        assert!(a.rewritten);
        assert_eq!(a.code, "(ns w.core)\n\n(defn w [input] [])\n");
    }

    #[test]
    fn an_ns_form_alone_changes_only_the_requires() {
        let edit = "(ns w.core\n  (:require [clojure.string :as str] [clojure.set :as set]))";
        let a = apply(BASE, edit, "w").unwrap();
        assert!(a.code.starts_with(edit), "{}", a.code);
        assert!(a.code.contains("(defn w [input]"));
    }

    #[test]
    fn unreadable_previous_code_needs_the_complete_namespace() {
        assert!(apply("(ns w.core)\n(defn w [x]", "(defn w [x] x)", "w").is_err());
        assert!(
            apply("(ns w.core)\n(defn w [x]", "(ns w.core)\n(defn w [x] x)", "w")
                .unwrap()
                .rewritten
        );
    }

    #[test]
    fn names_are_matched_as_whole_symbols() {
        assert!(uses("(map entry xs)", "entry"));
        assert!(!uses("(map entry-2 xs)", "entry"));
        assert!(!uses("(map :entry xs)", "entry"));
    }
}
