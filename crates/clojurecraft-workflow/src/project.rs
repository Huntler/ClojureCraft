//! Functions inside a project's namespace files.
//!
//! A project function is edited and generated as a small namespace of its own (the *fragment*):
//! the file's `ns` declaration with its requirements, the function's own private helpers, and
//! the function. The model only ever sees and writes that fragment. To check it, the fragment is
//! put back into the file in place of the function's previous definitions ([`compose`]), and
//! the whole file is loaded; only the fragment's definitions are held to the harness's rules.

use std::collections::BTreeSet;
use std::ops::Range;

use clojurecraft_clojure::{EvalRequest, ProjectEval};
use clojurecraft_domain::{FunctionDocument, ProjectBinding};
use clojurecraft_edn::{Delim, TokenKind};

use crate::structure::{Form, read_all};

/// A top-level definition in a namespace file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definition {
    /// `defn`, `defn-`, `def`, `defmulti`, …
    pub head: String,
    pub name: String,
    pub private: bool,
    /// The whole form, metadata included.
    pub span: Range<usize>,
    /// For a function: the parameters of its first arity that are required.
    pub arguments: Option<u32>,
}

impl Definition {
    /// A public function: what the project tree lists.
    pub fn is_public_function(&self) -> bool {
        self.head == "defn" && !self.private
    }

    fn is_function(&self) -> bool {
        matches!(self.head.as_str(), "defn" | "defn-")
    }
}

/// One library the `ns` declaration requires, and the requirement as written (prefix lists are
/// written out as one vector per library).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Require {
    pub lib: String,
    pub text: String,
}

/// The top-level structure of a namespace file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub namespace: Option<String>,
    ns_span: Option<Range<usize>>,
    /// The first `(:require …)` clause.
    require_clause: Option<Range<usize>>,
    pub requires: Vec<Require>,
    pub definitions: Vec<Definition>,
    /// Every top-level form after the `ns` declaration.
    forms: Vec<Range<usize>>,
}

/// Reads the top-level structure of `src`.
pub fn layout(src: &str) -> Result<Layout, String> {
    let forms = read_all(src).map_err(|(message, at)| {
        let (line, _) = clojurecraft_edn::line_col(src, at);
        format!("line {line}: {message}")
    })?;
    let mut out = Layout::default();
    for form in &forms {
        let items = form.list().unwrap_or_default();
        let head = items.first().and_then(Form::symbol).unwrap_or_default();
        if head == "ns" && out.ns_span.is_none() {
            out.namespace = items.get(1).and_then(Form::symbol).map(str::to_owned);
            out.ns_span = Some(full_span(form));
            read_requires(src, &items[2.min(items.len())..], &mut out);
            continue;
        }
        out.forms.push(full_span(form));
        let name_form = items.get(1);
        let name = name_form.and_then(Form::symbol);
        if let Some(name) = name
            && head.starts_with("def")
            && head != "defmethod"
        {
            let private = head == "defn-" || name_form.is_some_and(Form::is_private);
            out.definitions.push(Definition {
                head: head.to_owned(),
                name: name.to_owned(),
                private,
                span: full_span(form),
                arguments: matches!(head, "defn" | "defn-")
                    .then(|| first_arity(items))
                    .flatten(),
            });
        }
    }
    Ok(out)
}

/// The span of a form including any metadata in front of it.
fn full_span(form: &Form) -> Range<usize> {
    match form {
        Form::Meta { meta, target } => {
            let start = full_span(meta).start.saturating_sub(1);
            start..full_span(target).end
        }
        other => other.span(),
    }
}

fn read_requires(src: &str, clauses: &[Form], out: &mut Layout) {
    for clause in clauses {
        let parts = clause.list().unwrap_or_default();
        if parts.first().and_then(Form::keyword) != Some(":require") {
            continue;
        }
        if out.require_clause.is_none() {
            out.require_clause = Some(full_span(clause));
        }
        for spec in &parts[1..] {
            let text = &src[full_span(spec)];
            if let Some(lib) = spec.symbol() {
                out.requires.push(Require {
                    lib: lib.to_owned(),
                    text: text.to_owned(),
                });
            } else if let Some(lib) = spec.vector().and_then(|v| v.first()).and_then(Form::symbol) {
                out.requires.push(Require {
                    lib: lib.to_owned(),
                    text: text.to_owned(),
                });
            } else if let Some([prefix, rest @ ..]) = spec.list() {
                // (clojure [string :as str] set) → [clojure.string :as str] [clojure.set]
                let Some(prefix) = prefix.symbol() else { continue };
                for part in rest {
                    let inner = &src[full_span(part)];
                    if let Some(lib) = part.symbol() {
                        out.requires.push(Require {
                            lib: format!("{prefix}.{lib}"),
                            text: format!("[{prefix}.{lib}]"),
                        });
                    } else if let Some(lib) = part.vector().and_then(|v| v.first()).and_then(Form::symbol) {
                        out.requires.push(Require {
                            lib: format!("{prefix}.{lib}"),
                            text: format!("[{prefix}.{}", &inner[1..]),
                        });
                    }
                }
            }
        }
    }
}

/// Required parameters of the first arity of `(defn name doc? attrs? …)`.
fn first_arity(items: &[Form]) -> Option<u32> {
    let mut rest = items.get(2..).unwrap_or_default();
    while let Some(first) = rest.first() {
        let skip = matches!(
            first.bare(),
            Form::Atom {
                kind: TokenKind::String { .. },
                ..
            } | Form::Seq {
                delim: Delim::Brace,
                ..
            }
        );
        if !skip {
            break;
        }
        rest = &rest[1..];
    }
    let params = match rest.first()?.vector() {
        Some(params) => params,
        None => rest.first()?.list()?.first()?.vector()?,
    };
    let required = params.iter().take_while(|p| p.symbol() != Some("&")).count();
    u32::try_from(required).ok()
}

/// The public functions of a namespace file, with their argument counts, in file order.
pub fn public_functions(src: &str) -> Vec<(String, u32)> {
    let Ok(layout) = layout(src) else {
        return Vec::new();
    };
    let mut seen = BTreeSet::new();
    layout
        .definitions
        .iter()
        .filter(|d| d.is_public_function() && seen.insert(d.name.clone()))
        .map(|d| (d.name.clone(), d.arguments.unwrap_or(1)))
        .collect()
}

/// Namespaces the file requires.
pub fn file_requires(src: &str) -> Vec<String> {
    layout(src)
        .map(|l| l.requires.into_iter().map(|r| r.lib).collect())
        .unwrap_or_default()
}

/// Symbols that occur in `text`.
fn symbols(text: &str) -> BTreeSet<String> {
    clojurecraft_edn::tokenize(text)
        .into_iter()
        .filter(|t| t.kind == TokenKind::Symbol)
        .map(|t| text[t.span].to_owned())
        .collect()
}

/// The private functions that only `function` uses (directly or through each other): its own
/// helpers, rewritten with it. A private function another definition also uses is shared and
/// stays out.
pub fn owned_helpers(src: &str, function: &str) -> Vec<String> {
    let Ok(layout) = layout(src) else {
        return Vec::new();
    };
    let uses: Vec<(&Definition, BTreeSet<String>)> = layout
        .definitions
        .iter()
        .map(|d| (d, symbols(&src[d.span.clone()])))
        .collect();
    let private: BTreeSet<&str> = layout
        .definitions
        .iter()
        .filter(|d| d.private && d.is_function())
        .map(|d| d.name.as_str())
        .collect();
    // Private functions reachable from the function.
    let mut owned: BTreeSet<String> = BTreeSet::new();
    let mut frontier = vec![function.to_owned()];
    while let Some(name) = frontier.pop() {
        for (d, used) in uses.iter().filter(|(d, _)| d.name == name) {
            for u in used {
                if u != &d.name && private.contains(u.as_str()) && owned.insert(u.clone()) {
                    frontier.push(u.clone());
                }
            }
        }
    }
    // Drop helpers that something outside the function and its helpers also uses.
    loop {
        let shared: Vec<String> = owned
            .iter()
            .filter(|h| {
                uses.iter().any(|(d, used)| {
                    d.name != function && !owned.contains(&d.name) && d.name != **h && used.contains(*h)
                })
            })
            .cloned()
            .collect();
        if shared.is_empty() {
            break;
        }
        for h in shared {
            owned.remove(&h);
        }
    }
    layout
        .definitions
        .iter()
        .filter(|d| owned.contains(&d.name))
        .map(|d| d.name.clone())
        .fold(Vec::new(), |mut acc, n| {
            if !acc.contains(&n) {
                acc.push(n);
            }
            acc
        })
}

/// The `ns` declaration of a fragment: the file's namespace with all of its requirements.
fn fragment_ns(layout: &Layout, namespace: &str) -> String {
    if layout.requires.is_empty() {
        return format!("(ns {namespace})");
    }
    let specs: Vec<&str> = layout.requires.iter().map(|r| r.text.as_str()).collect();
    format!("(ns {namespace}\n  (:require {}))", specs.join("\n            "))
}

/// The function's fragment as it is in `binding.file`: the `ns` declaration, its helpers, and
/// the function. `None` when the file does not define the function (yet).
pub fn fragment(binding: &ProjectBinding) -> Option<String> {
    let layout = layout(&binding.file).ok()?;
    let owned = owned_names(binding);
    let defs: Vec<&Definition> = layout
        .definitions
        .iter()
        .filter(|d| owned.contains(d.name.as_str()))
        .collect();
    if !defs.iter().any(|d| d.name == binding.function) {
        return None;
    }
    let body: Vec<&str> = defs.iter().map(|d| &binding.file[d.span.clone()]).collect();
    Some(format!(
        "{}\n\n{}\n",
        fragment_ns(&layout, &binding.namespace),
        body.join("\n\n")
    ))
}

/// The `ns` declaration a new function's fragment starts with.
pub fn empty_fragment(binding: &ProjectBinding) -> String {
    let layout = layout(&binding.file).unwrap_or_default();
    fragment_ns(&layout, &binding.namespace)
}

fn owned_names(binding: &ProjectBinding) -> BTreeSet<&str> {
    std::iter::once(binding.function.as_str())
        .chain(binding.helpers.iter().map(String::as_str))
        .collect()
}

/// Lines of the fragment's definitions within the composed file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineMap {
    /// 1-based line where the definitions start in the composed file.
    pub file_start: u32,
    /// 1-based line where they start in the fragment.
    pub fragment_start: u32,
    pub count: u32,
}

impl LineMap {
    /// The fragment line of a line of the composed file; `None` outside the fragment's
    /// definitions.
    pub fn to_fragment(&self, line: u32) -> Option<u32> {
        (line >= self.file_start && line < self.file_start + self.count)
            .then(|| line - self.file_start + self.fragment_start)
    }
}

/// A fragment put back into its file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Composed {
    pub code: String,
    /// Every name the fragment defines.
    pub defined: Vec<String>,
    /// Its private definitions: the function's helpers from now on.
    pub helpers: Vec<String>,
    pub lines: LineMap,
}

fn line_of(src: &str, offset: usize) -> u32 {
    u32::try_from(src[..offset].matches('\n').count() + 1).unwrap_or(u32::MAX)
}

/// The end of the whitespace after `at`.
fn skip_space(src: &str, at: usize) -> usize {
    at + src[at..].len() - src[at..].trim_start().len()
}

/// `binding.file` with the function's definitions replaced by those of `fragment`, and any
/// requirement the fragment adds merged into the file's `ns` declaration. The rest of the file is
/// kept byte for byte.
pub fn compose(binding: &ProjectBinding, fragment: &str) -> Result<Composed, String> {
    let file = binding.file.as_str();
    let fl = layout(file).map_err(|e| format!("the namespace file cannot be read: {e}"))?;
    let fr = layout(fragment)?;
    let ns_span = fl
        .ns_span
        .clone()
        .ok_or("the namespace file has no ns declaration")?;
    let (Some(first), Some(last)) = (fr.forms.first(), fr.forms.last()) else {
        return Err("the code defines nothing".into());
    };
    let body_range = first.start..last.end;
    let body = &fragment[body_range.clone()];
    let owned = owned_names(binding);
    for d in &fr.definitions {
        if !owned.contains(d.name.as_str()) && fl.definitions.iter().any(|f| f.name == d.name) {
            return Err(format!(
                "{} is already defined elsewhere in {}; give the new definition another name",
                d.name, binding.namespace
            ));
        }
    }

    // (span to replace, replacement, whether the body goes here)
    let mut edits: Vec<(Range<usize>, String, bool)> = Vec::new();
    let known: BTreeSet<&str> = fl.requires.iter().map(|r| r.lib.as_str()).collect();
    let added: Vec<&str> = fr
        .requires
        .iter()
        .filter(|r| !known.contains(r.lib.as_str()))
        .map(|r| r.text.as_str())
        .collect();
    if !added.is_empty() {
        match &fl.require_clause {
            Some(clause) => {
                let at = clause.end - 1;
                let text: String = added.iter().map(|t| format!("\n            {t}")).collect();
                edits.push((at..at, text, false));
            }
            None => {
                let at = ns_span.end - 1;
                edits.push((
                    at..at,
                    format!("\n  (:require {})", added.join("\n            ")),
                    false,
                ));
            }
        }
    }
    let removed: Vec<&Definition> = fl
        .definitions
        .iter()
        .filter(|d| owned.contains(d.name.as_str()))
        .collect();
    let place = removed
        .iter()
        .position(|d| d.name == binding.function)
        .or((!removed.is_empty()).then_some(0));
    for (i, d) in removed.iter().enumerate() {
        let end = skip_space(file, d.span.end);
        if Some(i) == place {
            let tail = if end < file.len() { "\n\n" } else { "\n" };
            edits.push((d.span.start..end, tail.to_owned(), true));
        } else {
            edits.push((d.span.start..end, String::new(), false));
        }
    }
    if place.is_none() {
        let trimmed = file.trim_end().len();
        edits.push((trimmed..file.len(), "\n".to_owned(), true));
    }
    edits.sort_by_key(|(r, _, _)| r.start);

    let mut code = String::with_capacity(file.len() + body.len());
    let mut at = 0;
    let mut body_offset = 0;
    for (range, text, here) in edits {
        code.push_str(&file[at..range.start]);
        if here {
            if range.start == file.trim_end().len() {
                code.push_str("\n\n");
            }
            body_offset = code.len();
            code.push_str(body);
        }
        code.push_str(&text);
        at = range.end;
    }
    code.push_str(&file[at..]);

    Ok(Composed {
        defined: fr.definitions.iter().map(|d| d.name.clone()).collect(),
        helpers: fr
            .definitions
            .iter()
            .filter(|d| d.private)
            .map(|d| d.name.clone())
            .collect(),
        lines: LineMap {
            file_start: line_of(&code, body_offset),
            fragment_start: line_of(fragment, body_range.start),
            count: u32::try_from(body.matches('\n').count() + 1).unwrap_or(u32::MAX),
        },
        code,
    })
}

/// The rest of the namespace, as the model may use it: each other definition's signature and
/// docstring.
pub fn context(binding: &ProjectBinding) -> String {
    let Ok(layout) = layout(&binding.file) else {
        return String::new();
    };
    let Ok(forms) = read_all(&binding.file) else {
        return String::new();
    };
    let owned = owned_names(binding);
    let mut out = Vec::new();
    for form in forms.iter().skip(usize::from(layout.ns_span.is_some())) {
        let items = form.list().unwrap_or_default();
        let head = items.first().and_then(Form::symbol).unwrap_or_default();
        let Some(name) = items.get(1).and_then(Form::symbol) else {
            continue;
        };
        if owned.contains(name) || !head.starts_with("def") {
            continue;
        }
        let text = |f: &Form| binding.file[full_span(f)].to_owned();
        let mut line = format!("({head} {name}");
        let mut rest = &items[2..];
        let mut doc = None;
        if let Some(first) = rest.first()
            && matches!(
                first.bare(),
                Form::Atom {
                    kind: TokenKind::String { .. },
                    ..
                }
            )
        {
            doc = Some(text(first));
            rest = &rest[1..];
        }
        if matches!(head, "defn" | "defn-") {
            if rest.first().is_some_and(|f| {
                matches!(
                    f.bare(),
                    Form::Seq {
                        delim: Delim::Brace,
                        ..
                    }
                )
            }) {
                rest = &rest[1..];
            }
            let arglists: Vec<String> = match rest.first().and_then(Form::vector) {
                Some(_) => vec![text(&rest[0])],
                None => rest
                    .iter()
                    .filter_map(|a| a.list()?.first().filter(|p| p.vector().is_some()).map(text))
                    .collect(),
            };
            line.push(' ');
            line.push_str(&arglists.join(" "));
        }
        line.push(')');
        if let Some(doc) = doc {
            line.push_str(&format!(
                "\n  ;; {}",
                doc.trim_matches('"').replace('\n', "\n  ;; ")
            ));
        }
        out.push(line);
    }
    out.join("\n")
}

/// Turns a request for `doc`'s fragment into one for its whole file. Returns where the
/// fragment's lines are in the file; `None` for a function outside a project. Whether inputs are
/// argument vectors is left to the caller, since not every request calls the function itself.
pub fn prepare(doc: &FunctionDocument, request: &mut EvalRequest) -> Result<Option<LineMap>, String> {
    let Some(binding) = &doc.project else {
        return Ok(None);
    };
    let composed = compose(binding, &request.code)?;
    if let Some(probe) = &request.probe_code {
        request.probe_code = compose(binding, probe).ok().map(|c| c.code);
    }
    let namespaces = file_requires(&composed.code);
    request.code = composed.code;
    request.project = Some(ProjectEval {
        checked: composed.defined,
        namespaces,
    });
    Ok(Some(composed.lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "\
;; Orders.
(ns shop.orders
  (:require [clojure.string :as str]
            [shop.util :as util]))

(def tax 0.2)

(defn- clean [s] (str/trim s))

(defn- only-total [o] (:total o))

(defn label
  \"A label for an order.\"
  [order]
  (clean (:name order)))

(defn total [order discount]
  (- (only-total order) discount))

(defmulti kind :type)
";

    fn binding(function: &str, helpers: &[&str]) -> ProjectBinding {
        ProjectBinding {
            namespace: "shop.orders".into(),
            function: function.into(),
            helpers: helpers.iter().map(|h| (*h).to_owned()).collect(),
            arguments: 1,
            file: FILE.into(),
        }
    }

    #[test]
    fn the_layout_lists_requires_and_definitions() {
        let l = layout(FILE).expect("reads");
        assert_eq!(l.namespace.as_deref(), Some("shop.orders"));
        assert_eq!(
            l.requires.iter().map(|r| r.lib.as_str()).collect::<Vec<_>>(),
            ["clojure.string", "shop.util"]
        );
        assert_eq!(
            public_functions(FILE),
            [("label".to_owned(), 1), ("total".to_owned(), 2)]
        );
        let kind = l.definitions.iter().find(|d| d.name == "kind").expect("listed");
        assert_eq!(kind.head, "defmulti");
    }

    #[test]
    fn prefix_lists_are_written_out_per_library() {
        let l = layout("(ns a (:require (clojure [string :as str] set)))").expect("reads");
        assert_eq!(
            l.requires,
            vec![
                Require {
                    lib: "clojure.string".into(),
                    text: "[clojure.string :as str]".into()
                },
                Require {
                    lib: "clojure.set".into(),
                    text: "[clojure.set]".into()
                },
            ]
        );
    }

    #[test]
    fn helpers_used_only_by_the_function_belong_to_it() {
        assert_eq!(owned_helpers(FILE, "label"), ["clean"]);
        assert_eq!(owned_helpers(FILE, "total"), ["only-total"]);
        let shared = format!("{FILE}\n(defn other [x] (clean x))\n");
        assert!(owned_helpers(&shared, "label").is_empty());
        let chain = "(ns a)\n(defn- c [x] x)\n(defn- b [x] (c x))\n(defn f [x] (b x))\n";
        assert_eq!(owned_helpers(chain, "f"), ["c", "b"]);
    }

    #[test]
    fn the_fragment_holds_the_ns_the_helpers_and_the_function() {
        let f = fragment(&binding("label", &["clean"])).expect("defined");
        assert_eq!(
            f,
            "(ns shop.orders\n  (:require [clojure.string :as str]\n            [shop.util :as util]))\n\n\
             (defn- clean [s] (str/trim s))\n\n\
             (defn label\n  \"A label for an order.\"\n  [order]\n  (clean (:name order)))\n"
        );
        assert_eq!(fragment(&binding("missing", &[])), None);
    }

    #[test]
    fn composing_the_unchanged_fragment_gives_back_the_file() {
        let b = binding("label", &["clean"]);
        let f = fragment(&b).expect("defined");
        let c = compose(&b, &f).expect("composes");
        // The helper moves next to the function; everything else is unchanged.
        let expected = FILE.replace(
            "(defn- clean [s] (str/trim s))\n\n(defn- only-total",
            "(defn- only-total",
        );
        let expected = expected.replace("(defn label", "(defn- clean [s] (str/trim s))\n\n(defn label");
        assert_eq!(c.code, expected);
        assert_eq!(c.defined, ["clean", "label"]);
        assert_eq!(c.helpers, ["clean"]);
        let again = compose(
            &ProjectBinding {
                file: c.code.clone(),
                ..b
            },
            &f,
        )
        .expect("composes");
        assert_eq!(again.code, c.code, "composing is stable");
    }

    #[test]
    fn a_rewritten_function_replaces_its_old_definitions_and_adds_requires() {
        let b = binding("label", &["clean"]);
        let new = "(ns shop.orders\n  (:require [clojure.string :as str]\n            [clojure.set :as set]))\n\n\
                   (defn- shout [s] (str/upper-case s))\n\n(defn label [order]\n  (shout (:name order)))\n";
        let c = compose(&b, new).expect("composes");
        assert!(!c.code.contains("(defn- clean"));
        assert!(
            c.code
                .contains("(defn- shout [s] (str/upper-case s))\n\n(defn label [order]")
        );
        assert!(
            c.code
                .contains("[shop.util :as util]\n            [clojure.set :as set]))")
        );
        assert!(c.code.contains("(defn- only-total") && c.code.contains("(defmulti kind :type)"));
        assert_eq!(c.helpers, ["shout"]);
        let l = layout(&c.code).expect("composed code reads");
        assert_eq!(l.definitions.iter().filter(|d| d.name == "label").count(), 1);
    }

    #[test]
    fn lines_map_back_to_the_fragment() {
        let b = binding("total", &["only-total"]);
        let f = fragment(&b).expect("defined");
        let c = compose(&b, &f).expect("composes");
        let file_line = |needle: &str| line_of(&c.code, c.code.find(needle).expect("present"));
        let fragment_line = |needle: &str| line_of(&f, f.find(needle).expect("present"));
        let at = "(- (only-total order) discount)";
        assert_eq!(c.lines.to_fragment(file_line(at)), Some(fragment_line(at)));
        assert_eq!(c.lines.to_fragment(file_line("(def tax")), None);
    }

    #[test]
    fn a_new_function_is_appended() {
        let b = binding("discounted", &[]);
        let f = "(ns shop.orders)\n\n(defn discounted [o] (* 0.9 (total o 0)))\n";
        let c = compose(&b, f).expect("composes");
        assert!(c.code.starts_with(FILE.trim_end()));
        assert!(
            c.code
                .ends_with("(defmulti kind :type)\n\n(defn discounted [o] (* 0.9 (total o 0)))\n")
        );
        let fresh = ProjectBinding {
            file: "(ns shop.empty)\n".into(),
            namespace: "shop.empty".into(),
            ..b
        };
        let c = compose(&fresh, "(ns shop.empty)\n(defn discounted [o] o)").expect("composes");
        assert_eq!(c.code, "(ns shop.empty)\n\n(defn discounted [o] o)\n");
        assert_eq!(empty_fragment(&fresh), "(ns shop.empty)");
    }

    #[test]
    fn a_helper_may_not_redefine_another_definition_of_the_file() {
        let b = binding("label", &["clean"]);
        let f = "(ns shop.orders)\n(defn- only-total [o] 0)\n(defn label [o] o)\n";
        let e = compose(&b, f).expect_err("conflict");
        assert!(e.contains("only-total is already defined"), "{e}");
    }

    #[test]
    fn the_context_lists_the_other_definitions() {
        let c = context(&binding("label", &["clean"]));
        assert_eq!(
            c,
            "(def tax)\n(defn- only-total [o])\n(defn total [order discount])\n(defmulti kind)"
        );
        let c = context(&binding("total", &["only-total"]));
        assert!(
            c.contains("(defn label [order])\n  ;; A label for an order."),
            "{c}"
        );
    }

    #[test]
    fn prepare_turns_a_fragment_request_into_a_whole_file_request() {
        let mut doc = FunctionDocument::new("total");
        let mut b = binding("total", &["only-total"]);
        b.arguments = 2;
        let f = fragment(&b).expect("defined");
        doc.project = Some(b);
        let mut r = EvalRequest::new(f.clone(), "total", vec![]);
        r.probe_code = Some(f.replace("(only-total order)", "(identity (only-total order))"));
        let map = prepare(&doc, &mut r).expect("prepares").expect("project");
        assert!(r.code.contains("(defn label"));
        let p = r.project.as_ref().expect("project mode");
        assert_eq!(p.checked, ["only-total", "total"]);
        assert_eq!(p.namespaces, ["clojure.string", "shop.util"]);
        assert!(map.count > 0);
        assert!(
            r.probe_code
                .as_deref()
                .is_some_and(|c| c.contains("(defn label") && c.contains("(identity"))
        );
        let mut plain = EvalRequest::new("(ns a)", "f", vec![]);
        assert_eq!(prepare(&FunctionDocument::new("f"), &mut plain), Ok(None));
        assert!(plain.project.is_none());
    }
}
