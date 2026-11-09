//! Structural checks of generated Clojure source, made before the code is ever executed.
//!
//! The source is read into forms with the same tolerant lexer the editors use, then checked
//! against the rules of the evaluation harness (read from the harness itself): one `ns` form with
//! only `:require` clauses of supported namespaces, top-level forms limited to definitions,
//! exactly one public function that accepts one argument, and no symbols the harness refuses
//! (dynamic evaluation, state, concurrency, I/O, Java interop outside a few numeric classes).
//!
//! These checks give fast, precise feedback. They are not a security boundary: the harness repeats
//! them inside the operating-system sandbox, which is what actually contains the code.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use clojurecraft_edn::{Delim, TokenKind, line_col, tokenize};

use crate::deps::{self, CORE};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProblemKind {
    /// The text cannot be read as Clojure.
    Syntax,
    /// Readable, but not shaped like one namespace with one public function.
    Structure,
    /// Uses something the harness refuses.
    Forbidden,
    /// Requires or refers to a namespace or class that is not supported at all.
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub kind: ProblemKind,
    pub message: String,
    /// 1-based line, when the problem has a position.
    pub line: Option<usize>,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(l) => write!(f, "line {l}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// What the source defines, and what is wrong with it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodeStructure {
    pub namespace: Option<String>,
    /// The public function, when there is exactly one.
    pub function: Option<String>,
    /// Required namespaces besides `clojure.core`, in order of appearance.
    pub requires: Vec<String>,
    /// Supported namespaces the code requires that this function is not yet allowed to use.
    pub needs_approval: Vec<String>,
    pub problems: Vec<Problem>,
    /// Deviations that do not prevent running the code, such as a different function name.
    pub warnings: Vec<String>,
}

impl CodeStructure {
    /// Valid, and every required namespace is allowed.
    pub fn is_acceptable(&self) -> bool {
        self.problems.is_empty() && self.needs_approval.is_empty()
    }
}

/// What the code is checked against.
#[derive(Clone, Debug)]
pub struct Expectations<'a> {
    pub namespace: &'a str,
    pub function: &'a str,
    /// Namespaces besides `clojure.core` this function may require.
    pub allowed: &'a BTreeSet<String>,
    /// How many arguments the public function takes.
    pub arguments: u32,
}

#[derive(Clone, Debug)]
pub(crate) enum Form {
    Seq {
        delim: Delim,
        items: Vec<Form>,
        span: Range<usize>,
    },
    Atom {
        kind: TokenKind,
        text: String,
        span: Range<usize>,
    },
    /// A reader macro or tag applied to the next form (`'x`, `#(...)`, `@x`, `#inst "…"`).
    Prefixed {
        prefix: String,
        inner: Box<Form>,
        span: Range<usize>,
    },
    /// `^meta target`
    Meta { meta: Box<Form>, target: Box<Form> },
}

impl Form {
    pub(crate) fn span(&self) -> Range<usize> {
        match self {
            Self::Seq { span, .. } | Self::Atom { span, .. } | Self::Prefixed { span, .. } => span.clone(),
            Self::Meta { target, .. } => target.span(),
        }
    }

    /// The form without metadata.
    pub(crate) fn bare(&self) -> &Form {
        match self {
            Self::Meta { target, .. } => target.bare(),
            other => other,
        }
    }

    pub(crate) fn symbol(&self) -> Option<&str> {
        match self.bare() {
            Self::Atom {
                kind: TokenKind::Symbol,
                text,
                ..
            } => Some(text),
            _ => None,
        }
    }

    pub(crate) fn keyword(&self) -> Option<&str> {
        match self.bare() {
            Self::Atom {
                kind: TokenKind::Keyword,
                text,
                ..
            } => Some(text),
            _ => None,
        }
    }

    pub(crate) fn list(&self) -> Option<&[Form]> {
        match self.bare() {
            Self::Seq {
                delim: Delim::Paren,
                items,
                ..
            } => Some(items),
            _ => None,
        }
    }

    pub(crate) fn vector(&self) -> Option<&[Form]> {
        match self.bare() {
            Self::Seq {
                delim: Delim::Bracket,
                items,
                ..
            } => Some(items),
            _ => None,
        }
    }

    fn is_string(&self) -> bool {
        matches!(
            self.bare(),
            Self::Atom {
                kind: TokenKind::String { .. },
                ..
            }
        )
    }

    /// Whether metadata on this form marks it private (`^:private` or `^{:private true}`).
    pub(crate) fn is_private(&self) -> bool {
        match self {
            Self::Meta { meta, target } => {
                let here = match meta.bare() {
                    Self::Atom { text, .. } => text == ":private",
                    Self::Seq {
                        delim: Delim::Brace,
                        items,
                        ..
                    } => items.chunks(2).any(|kv| {
                        kv[0].keyword() == Some(":private")
                            && kv.get(1).and_then(Form::symbol).is_none_or(|v| v != "false")
                    }),
                    _ => false,
                };
                here || target.is_private()
            }
            _ => false,
        }
    }
}

struct Reader<'a> {
    src: &'a str,
    tokens: Vec<clojurecraft_edn::Token>,
    pos: usize,
}

pub(crate) type ReadError = (String, usize);

impl Reader<'_> {
    fn skip_trivia(&mut self) {
        while self
            .tokens
            .get(self.pos)
            .is_some_and(|t| matches!(t.kind, TokenKind::Whitespace | TokenKind::Comment))
        {
            self.pos += 1;
        }
    }

    fn text(&self, span: &Range<usize>) -> String {
        self.src[span.clone()].to_owned()
    }

    /// The next form, or `None` at the end of input or before a closing delimiter.
    fn form(&mut self) -> Result<Option<Form>, ReadError> {
        loop {
            self.skip_trivia();
            let Some(token) = self.tokens.get(self.pos).cloned() else {
                return Ok(None);
            };
            let start = token.span.start;
            match token.kind {
                TokenKind::Close(_) => return Ok(None),
                TokenKind::Discard => {
                    self.pos += 1;
                    self.required(start, "#_")?;
                }
                TokenKind::Open(delim) => {
                    self.pos += 1;
                    let mut items = Vec::new();
                    while let Some(f) = self.form()? {
                        items.push(f);
                    }
                    let Some(close) = self.tokens.get(self.pos).cloned() else {
                        return Err((format!("{} is never closed", delim.open()), start));
                    };
                    let TokenKind::Close(c) = close.kind else {
                        return Err(("unexpected input".into(), close.span.start));
                    };
                    if !delim.closed_by(c) {
                        return Err((
                            format!("{} is closed by {}", delim.open(), c.close()),
                            close.span.start,
                        ));
                    }
                    self.pos += 1;
                    if delim == Delim::Brace && items.len() % 2 == 1 {
                        return Err(("a map needs an even number of forms".into(), start));
                    }
                    return Ok(Some(Form::Seq {
                        delim,
                        items,
                        span: start..close.span.end,
                    }));
                }
                TokenKind::String { terminated: false } | TokenKind::Regex { terminated: false } => {
                    return Err(("the string is never closed".into(), start));
                }
                TokenKind::Invalid => {
                    return Err((format!("cannot read {:?}", self.text(&token.span)), start));
                }
                TokenKind::ReaderMacro if &self.src[token.span.clone()] == "^" => {
                    self.pos += 1;
                    let meta = self.required(start, "^")?;
                    let target = self.required(start, "^")?;
                    return Ok(Some(Form::Meta {
                        meta: Box::new(meta),
                        target: Box::new(target),
                    }));
                }
                TokenKind::ReaderMacro | TokenKind::Tag | TokenKind::NamespacePrefix => {
                    self.pos += 1;
                    let prefix = self.text(&token.span);
                    let inner = self.required(start, &prefix)?;
                    let end = inner.span().end;
                    return Ok(Some(Form::Prefixed {
                        prefix,
                        inner: Box::new(inner),
                        span: start..end,
                    }));
                }
                kind => {
                    self.pos += 1;
                    return Ok(Some(Form::Atom {
                        kind,
                        text: self.text(&token.span),
                        span: token.span,
                    }));
                }
            }
        }
    }

    fn required(&mut self, at: usize, after: &str) -> Result<Form, ReadError> {
        self.form()?
            .ok_or_else(|| (format!("{after} must be followed by a form"), at))
    }
}

pub(crate) fn read_all(src: &str) -> Result<Vec<Form>, ReadError> {
    let mut r = Reader {
        src,
        tokens: tokenize(src),
        pos: 0,
    };
    let mut forms = Vec::new();
    loop {
        if let Some(f) = r.form()? {
            forms.push(f);
            continue;
        }
        match r.tokens.get(r.pos) {
            None => return Ok(forms),
            Some(t) => return Err((format!("unmatched {}", &src[t.span.clone()]), t.span.start)),
        }
    }
}

struct Checker<'a> {
    src: &'a str,
    out: CodeStructure,
    /// Alias or full name → namespace, for required namespaces.
    aliases: BTreeMap<String, String>,
    allowed: &'a BTreeSet<String>,
}

impl Checker<'_> {
    fn line(&self, at: usize) -> usize {
        line_col(self.src, at).0
    }

    fn problem(&mut self, kind: ProblemKind, message: impl Into<String>, at: Option<usize>) {
        let message = message.into();
        let line = at.map(|a| self.line(a));
        if !self
            .out
            .problems
            .iter()
            .any(|p| p.message == message && p.line == line)
        {
            self.out.problems.push(Problem { kind, message, line });
        }
    }

    fn ns_form(&mut self, form: &Form, expected: &str) {
        let items = form.list().unwrap_or_default();
        let at = Some(form.span().start);
        if items.first().and_then(Form::symbol) != Some("ns") {
            self.problem(
                ProblemKind::Structure,
                "the code must start with an (ns …) declaration",
                at,
            );
            return;
        }
        let Some(name) = items.get(1).and_then(Form::symbol) else {
            self.problem(ProblemKind::Structure, "the ns declaration needs a name", at);
            return;
        };
        if name != expected {
            self.out
                .warnings
                .push(format!("the namespace is {name} instead of {expected}"));
        }
        self.out.namespace = Some(name.to_owned());
        for clause in &items[2..] {
            if clause.is_string()
                || matches!(
                    clause.bare(),
                    Form::Seq {
                        delim: Delim::Brace,
                        ..
                    }
                )
            {
                continue;
            }
            let at = Some(clause.span().start);
            let parts = clause.list().unwrap_or_default();
            match parts.first().and_then(Form::keyword) {
                Some(":require") => {
                    for spec in &parts[1..] {
                        self.require_spec(spec);
                    }
                }
                Some(other) => self.problem(
                    ProblemKind::Forbidden,
                    format!("only :require is allowed in the ns declaration, not {other}"),
                    at,
                ),
                None => self.problem(
                    ProblemKind::Structure,
                    "unexpected form in the ns declaration",
                    at,
                ),
            }
        }
    }

    fn require_spec(&mut self, spec: &Form) {
        let at = Some(spec.span().start);
        let (lib, opts) = match (spec.symbol(), spec.vector()) {
            (Some(lib), _) => (lib.to_owned(), &[][..]),
            (None, Some([first, rest @ ..])) if first.symbol().is_some() => {
                (first.symbol().unwrap_or_default().to_owned(), rest)
            }
            _ => {
                self.problem(
                    ProblemKind::Structure,
                    "write requirements as [namespace :as alias]",
                    at,
                );
                return;
            }
        };
        // A project namespace may use whatever its file requires; only the standard library's
        // requirements are held to the usual rules.
        let project_lib = !deps::is_supported(&lib) && self.allowed.contains(&lib);
        if !deps::is_supported(&lib) && !project_lib {
            self.problem(
                ProblemKind::Unsupported,
                format!("{lib} is not available to generated functions"),
                at,
            );
            return;
        }
        if lib != CORE && !self.out.requires.contains(&lib) {
            self.out.requires.push(lib.clone());
            if !self.allowed.contains(&lib) {
                self.out.needs_approval.push(lib.clone());
            }
        }
        self.aliases.insert(lib.clone(), lib.clone());
        for pair in opts.chunks(2) {
            match (pair[0].keyword(), pair.get(1)) {
                (Some(":as" | ":as-alias"), Some(alias)) => {
                    if let Some(a) = alias.symbol() {
                        self.aliases.insert(a.to_owned(), lib.clone());
                    }
                }
                (Some(":refer"), Some(names)) if names.vector().is_some() => {}
                _ if project_lib => {}
                (Some(":refer"), _) => self.problem(
                    ProblemKind::Forbidden,
                    ":refer :all is not allowed; use an alias or a vector of names",
                    at,
                ),
                _ => self.problem(
                    ProblemKind::Structure,
                    format!("unexpected option in [{lib} …]"),
                    at,
                ),
            }
        }
    }

    fn top_level(&mut self, form: &Form) -> Option<(String, bool, Form)> {
        let at = Some(form.span().start);
        let policy = clojurecraft_clojure::harness_policy();
        let Some(items) = form.list() else {
            self.problem(
                ProblemKind::Structure,
                "only defn, defn-, def and declare are allowed at top level",
                at,
            );
            return None;
        };
        let head = items.first().and_then(Form::symbol).unwrap_or("");
        if !policy.allowed_top_level.contains(head) {
            self.problem(
                ProblemKind::Structure,
                format!(
                    "only defn, defn-, def and declare are allowed at top level{}",
                    if head.is_empty() {
                        String::new()
                    } else {
                        format!(", not {head}")
                    }
                ),
                at,
            );
            return None;
        }
        let Some(name_form) = items.get(1) else {
            self.problem(ProblemKind::Structure, format!("{head} needs a name"), at);
            return None;
        };
        let Some(name) = name_form.symbol() else {
            if head != "declare" {
                self.problem(ProblemKind::Structure, format!("{head} needs a name"), at);
            }
            return None;
        };
        let public_fn = head == "defn" && !name_form.is_private();
        Some((name.to_owned(), public_fn, form.clone()))
    }

    /// Whether some arity of `(defn name doc? attrs? …)` takes exactly `n` arguments.
    fn takes_arguments(defn: &Form, n: u32) -> bool {
        let n = usize::try_from(n).unwrap_or(usize::MAX);
        let items = defn.list().unwrap_or_default();
        let mut rest = items.get(2..).unwrap_or_default();
        while let Some(first) = rest.first() {
            if first.is_string()
                || matches!(
                    first.bare(),
                    Form::Seq {
                        delim: Delim::Brace,
                        ..
                    }
                )
            {
                rest = &rest[1..];
            } else {
                break;
            }
        }
        let accepts = |params: &[Form]| {
            let required = params.iter().take_while(|p| p.symbol() != Some("&")).count();
            let variadic = required < params.len();
            required == n || (variadic && required <= n)
        };
        match rest.first() {
            Some(f) if f.vector().is_some() => accepts(f.vector().unwrap_or_default()),
            _ => rest
                .iter()
                .filter_map(|arity| arity.list()?.first()?.vector())
                .any(accepts),
        }
    }

    fn symbols(&mut self, form: &Form) {
        match form {
            Form::Seq { items, .. } => {
                if form.list().and_then(|l| l.first()).and_then(Form::symbol) == Some("quote") {
                    return;
                }
                for f in items {
                    self.symbols(f);
                }
            }
            Form::Meta { target, .. } => self.symbols(target),
            Form::Prefixed { prefix, inner, span } => match prefix.as_str() {
                "'" => {}
                "#'" => self.problem(
                    ProblemKind::Forbidden,
                    "var references (#') are not allowed",
                    Some(span.start),
                ),
                "#=" => self.problem(
                    ProblemKind::Forbidden,
                    "read-time evaluation (#=) is not allowed",
                    Some(span.start),
                ),
                _ => self.symbols(inner),
            },
            Form::Atom {
                kind: TokenKind::Symbol,
                text,
                span,
            } => self.symbol(text, span.start),
            Form::Atom { .. } => {}
        }
    }

    fn symbol(&mut self, sym: &str, at: usize) {
        let policy = clojurecraft_clojure::harness_policy();
        let at = Some(at);
        let (ns, name) = match sym.split_once('/') {
            Some((ns, name)) if !ns.is_empty() && !name.is_empty() => (Some(ns), name),
            _ => (None, sym),
        };
        let Some(ns) = ns else {
            if policy.denied_core.contains(sym) {
                self.problem(ProblemKind::Forbidden, format!("{sym} is not allowed"), at);
            } else if sym != "." && (sym.starts_with('.') || sym.ends_with('.')) {
                self.problem(
                    ProblemKind::Forbidden,
                    format!("Java interop ({sym}) is not allowed"),
                    at,
                );
            } else if sym.contains('.') && sym != "." {
                self.problem(
                    ProblemKind::Forbidden,
                    format!("class or namespace reference {sym} is not allowed"),
                    at,
                );
            }
            return;
        };
        if policy.allowed_classes.contains(ns) {
            return;
        }
        let target = self.aliases.get(ns).cloned().unwrap_or_else(|| ns.to_owned());
        if target == CORE {
            if policy.denied_core.contains(name) {
                self.problem(ProblemKind::Forbidden, format!("{sym} is not allowed"), at);
            }
        } else if !deps::is_supported(&target) && !self.allowed.contains(&target) {
            self.problem(
                ProblemKind::Unsupported,
                format!("{ns} is not available to generated functions ({sym})"),
                at,
            );
        } else if !self.aliases.contains_key(ns) {
            self.problem(
                ProblemKind::Structure,
                format!("{sym} uses {target} without requiring it in the ns declaration"),
                at,
            );
        }
    }
}

/// Reads and checks `source`.
pub fn check(source: &str, expect: &Expectations<'_>) -> CodeStructure {
    let mut c = Checker {
        src: source,
        out: CodeStructure::default(),
        aliases: BTreeMap::from([(CORE.to_owned(), CORE.to_owned())]),
        allowed: expect.allowed,
    };
    let forms = match read_all(source) {
        Ok(f) => f,
        Err((message, at)) => {
            c.problem(ProblemKind::Syntax, message, Some(at));
            return c.out;
        }
    };
    let Some((ns, rest)) = forms.split_first() else {
        c.problem(ProblemKind::Structure, "the code is empty", None);
        return c.out;
    };
    c.ns_form(ns, expect.namespace);
    let mut public = Vec::new();
    for form in rest {
        if let Some((name, true, form)) = c.top_level(form) {
            public.push((name, form));
        }
        c.symbols(form);
    }
    match public.as_slice() {
        [] => c.problem(
            ProblemKind::Structure,
            format!(
                "no public function is defined; define ({} [input] …) with defn",
                expect.function
            ),
            None,
        ),
        [(name, form)] => {
            if !Checker::takes_arguments(form, expect.arguments) {
                let message = match expect.arguments {
                    1 => format!("{name} must accept exactly one argument, the input value"),
                    n => format!("{name} must accept exactly {n} arguments"),
                };
                c.problem(ProblemKind::Structure, message, Some(form.span().start));
            }
            if name != expect.function {
                c.out.warnings.push(format!(
                    "the function is named {name} instead of {}",
                    expect.function
                ));
            }
            c.out.function = Some(name.clone());
        }
        many => c.problem(
            ProblemKind::Structure,
            format!(
                "{} public functions are defined ({}); define helpers with defn-",
                many.len(),
                many.iter()
                    .map(|(n, _)| n.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None,
        ),
    }
    c.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed() -> BTreeSet<String> {
        BTreeSet::from(["clojure.string".into(), "clojure.set".into()])
    }

    fn run(src: &str) -> CodeStructure {
        let allowed = allowed();
        check(
            src,
            &Expectations {
                namespace: "totals.core",
                function: "totals",
                allowed: &allowed,
                arguments: 1,
            },
        )
    }

    fn messages(s: &CodeStructure) -> Vec<String> {
        s.problems.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn the_expected_number_of_arguments_is_checked() {
        let allowed = allowed();
        let with = |src: &str, arguments| {
            check(
                src,
                &Expectations {
                    namespace: "totals.core",
                    function: "totals",
                    allowed: &allowed,
                    arguments,
                },
            )
        };
        assert!(
            with("(ns totals.core)\n(defn totals [a b] a)", 2)
                .problems
                .is_empty()
        );
        assert!(
            with("(ns totals.core)\n(defn totals [a & more] a)", 3)
                .problems
                .is_empty()
        );
        assert_eq!(
            messages(&with("(ns totals.core)\n(defn totals [a] a)", 2)),
            ["line 2: totals must accept exactly 2 arguments"]
        );
        assert!(
            with("(ns totals.core)\n(defn totals [] 1)", 0)
                .problems
                .is_empty()
        );
    }

    #[test]
    fn a_project_namespace_may_use_what_its_file_requires() {
        let allowed = BTreeSet::from(["shop.util".to_owned(), "clojure.string".to_owned()]);
        let s = check(
            "(ns shop.orders\n  (:require [shop.util :as util :refer :all]\n            [clojure.string :as str]))\n             (defn totals [x] (util/clean (str/trim x)))",
            &Expectations {
                namespace: "shop.orders",
                function: "totals",
                allowed: &allowed,
                arguments: 1,
            },
        );
        assert!(s.problems.is_empty(), "{:?}", s.problems);
        assert!(s.needs_approval.is_empty());
        let s = run("(ns totals.core (:require [shop.util :as util]))\n(defn totals [x] (util/clean x))");
        assert!(
            !s.problems.is_empty(),
            "outside a project, other namespaces stay unsupported"
        );
    }

    #[test]
    fn a_well_formed_namespace_is_accepted() {
        let s = run(
            "(ns totals.core\n  \"Totals.\"\n  (:require [clojure.string :as str]))\n\n\
             (defn- amount [o] (or (:amount o) 0))\n\
             (def ^:private zero 0)\n\
             (defn totals\n  \"Sums.\"\n  [orders]\n  (->> orders (map amount) (reduce + zero) (Math/abs) str/trim))",
        );
        assert!(s.is_acceptable(), "{:?}", s.problems);
        assert_eq!(s.namespace.as_deref(), Some("totals.core"));
        assert_eq!(s.function.as_deref(), Some("totals"));
        assert_eq!(s.requires, ["clojure.string"]);
        assert!(s.warnings.is_empty());
    }

    #[test]
    fn syntax_errors_are_positioned() {
        let s = run("(ns totals.core)\n(defn totals [x]\n  (inc x)");
        assert_eq!(s.problems[0].kind, ProblemKind::Syntax);
        assert_eq!(s.problems[0].line, Some(2));
        assert!(messages(&s)[0].contains("never closed"));
        let s = run("(ns totals.core)\n(defn totals [x] x))");
        assert!(messages(&s)[0].contains("unmatched )"), "{:?}", s.problems);
        let s = run("(ns totals.core)\n(defn totals [x] (str \"x))");
        assert!(messages(&s)[0].contains("string is never closed"));
    }

    #[test]
    fn exactly_one_public_one_argument_function_is_required() {
        let s = run("(ns totals.core)\n(defn- helper [x] x)");
        assert!(messages(&s)[0].contains("no public function"));
        let s = run("(ns totals.core)\n(defn a [x] x)\n(defn b [x] x)");
        assert!(messages(&s)[0].contains("2 public functions are defined (a, b)"));
        let s = run("(ns totals.core)\n(defn ^:private a [x] x)\n(defn totals [x y] x)");
        assert!(
            messages(&s)[0].contains("must accept exactly one argument"),
            "{:?}",
            s.problems
        );
        for ok in [
            "(defn totals [{:keys [a]}] a)",
            "(defn totals ([] 0) ([x] x))",
            "(defn totals [& xs] xs)",
            "(defn totals \"doc\" {:added \"1\"} [x] x)",
        ] {
            let s = run(&format!("(ns totals.core)\n{ok}"));
            assert!(s.is_acceptable(), "{ok}: {:?}", s.problems);
        }
    }

    #[test]
    fn other_names_are_accepted_with_a_warning() {
        let s = run("(ns my.ns)\n(defn sum-up [x] x)");
        assert!(s.is_acceptable());
        assert_eq!(s.function.as_deref(), Some("sum-up"));
        assert_eq!(s.warnings.len(), 2);
    }

    #[test]
    fn harness_restrictions_are_reported_before_execution() {
        let cases = [
            ("(defn totals [x] (eval x))", "eval is not allowed"),
            ("(defn totals [x] (slurp x))", "slurp is not allowed"),
            (
                "(defn totals [x] (clojure.core/eval x))",
                "clojure.core/eval is not allowed",
            ),
            ("(defn totals [x] (.toUpperCase x))", "Java interop"),
            (
                "(defn totals [x] (java.util.UUID/randomUUID))",
                "java.util.UUID is not available",
            ),
            ("(defn totals [x] (System/exit 1))", "System is not available"),
            ("(defn totals [x] (String. x))", "Java interop"),
            ("(defn totals [x] #'inc)", "var references"),
            ("(defn totals [x] (future x))", "future is not allowed"),
            ("(println 1)", "only defn, defn-, def and declare"),
            (
                "(defn totals [x] (clojure.walk/keywordize-keys x))",
                "without requiring",
            ),
        ];
        for (code, needle) in cases {
            let s = run(&format!("(ns totals.core)\n{code}"));
            assert!(
                messages(&s).iter().any(|m| m.contains(needle)),
                "{code}: {:?}",
                s.problems
            );
        }
        let s = run("(ns totals.core)\n(defn totals [x] (if x 'eval '(slurp \"f\")))");
        assert!(s.is_acceptable(), "quoted data is not code: {:?}", s.problems);
        let s = run("(ns totals.core)\n(defn totals [x] (Math/abs (Long/parseLong x)))");
        assert!(s.is_acceptable(), "{:?}", s.problems);
    }

    #[test]
    fn namespaces_are_supported_allowed_or_need_approval() {
        let s = run(
            "(ns totals.core (:require [clojure.walk :as w] [clojure.set :as set]))\n(defn totals [x] (w/keywordize-keys (set/union x)))",
        );
        assert!(s.problems.is_empty(), "{:?}", s.problems);
        assert_eq!(s.requires, ["clojure.walk", "clojure.set"]);
        assert_eq!(s.needs_approval, ["clojure.walk"]);
        assert!(!s.is_acceptable());

        let s = run("(ns totals.core (:require [clojure.java.io :as io]))\n(defn totals [x] x)");
        assert_eq!(s.problems[0].kind, ProblemKind::Unsupported);
        let s = run("(ns totals.core (:use clojure.string))\n(defn totals [x] x)");
        assert!(messages(&s)[0].contains("only :require"));
        let s = run("(ns totals.core (:require [clojure.string :refer :all]))\n(defn totals [x] x)");
        assert!(messages(&s)[0].contains(":refer :all"));
        let s = run("(ns totals.core (:require clojure.string))\n(defn totals [x] (clojure.string/trim x))");
        assert!(s.is_acceptable(), "{:?}", s.problems);
    }

    #[test]
    fn the_namespace_must_come_first() {
        let s = run("(defn totals [x] x)");
        assert!(messages(&s)[0].contains("must start with an (ns"));
        assert!(messages(&run("")).iter().any(|m| m.contains("empty")));
    }
}
