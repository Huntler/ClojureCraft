//! What the project index holds: one chunk per public definition of the project's namespaces
//! and of the Clojure sources in its dependencies, so a generation can find and reuse code that
//! already exists.
//!
//! A chunk shows the definition's signature and docstring, or its source when it has no
//! docstring. Definitions that break the harness's rules for generated code (I/O, mutation),
//! or that call such definitions, are marked restricted: they are never offered to generation,
//! but stay available for describing code.
//!
//! The namespace graph decides which namespaces a function may require without creating a
//! circular dependency.

use std::collections::{BTreeMap, BTreeSet};

use clojurecraft_edn::TokenKind;
use clojurecraft_knowledge::{Chunk, ChunkKind, chunk::SourceKey};

use crate::project::{full_span, layout, summarize};
use crate::structure::{Form, read_all};
use crate::{deps, project};

/// Longest source shown for a definition without a docstring.
pub const MAX_SOURCE_CHARS: usize = 1500;

/// One Clojure source file to index.
#[derive(Clone, Debug)]
pub struct SourceFile {
    /// Where the file is, as shown with its chunks (a path, or `jar!/entry`).
    pub location: String,
    pub text: String,
    /// The file comes from a dependency rather than the project's own sources.
    pub library: bool,
}

/// The chunks of a set of source files and how their namespaces depend on each other.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectIndex {
    pub chunks: Vec<Chunk>,
    /// Each indexed namespace and the indexed namespaces it requires.
    pub graph: BTreeMap<String, BTreeSet<String>>,
    /// Namespaces from dependencies.
    pub library: BTreeSet<String>,
    /// Files that could not be read as Clojure.
    pub unreadable: Vec<String>,
}

struct Def<'a> {
    namespace: String,
    name: String,
    head: String,
    private: bool,
    form: &'a Form,
}

/// Which names a namespace file refers to: aliases and referred symbols.
#[derive(Default)]
struct Names {
    aliases: BTreeMap<String, String>,
    referred: BTreeMap<String, String>,
}

fn names_of(text: &str) -> Names {
    let mut out = Names::default();
    let Ok(layout) = layout(text) else {
        return out;
    };
    for r in layout.requires {
        out.aliases.insert(r.lib.clone(), r.lib.clone());
        let tokens: Vec<clojurecraft_edn::Token> = clojurecraft_edn::tokenize(&r.text)
            .into_iter()
            .filter(|t| !matches!(t.kind, TokenKind::Whitespace | TokenKind::Comment))
            .collect();
        let word = |i: usize| tokens.get(i).map(|t| &r.text[t.span.clone()]);
        for (i, t) in tokens.iter().enumerate() {
            match &r.text[t.span.clone()] {
                ":as" | ":as-alias" => {
                    if let Some(a) = word(i + 1) {
                        out.aliases.insert(a.to_owned(), r.lib.clone());
                    }
                }
                ":refer" => {
                    for t in tokens[i + 1..].iter().skip(1) {
                        if t.kind != TokenKind::Symbol {
                            break;
                        }
                        out.referred
                            .insert(r.text[t.span.clone()].to_owned(), r.lib.clone());
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// What a definition's code refers to, and whether it does something impure itself.
#[derive(Default)]
struct Uses {
    impure: bool,
    /// Qualified `namespace/name` of other definitions it may call.
    calls: BTreeSet<String>,
}

fn uses(
    form: &Form,
    namespace: &str,
    names: &Names,
    own: &BTreeSet<String>,
    indexed: &BTreeSet<String>,
) -> Uses {
    let mut u = Uses::default();
    walk(form, namespace, names, own, indexed, &mut u);
    u
}

fn walk(
    form: &Form,
    namespace: &str,
    names: &Names,
    own: &BTreeSet<String>,
    indexed: &BTreeSet<String>,
    u: &mut Uses,
) {
    match form {
        Form::Seq { items, .. } => {
            if form.list().and_then(|l| l.first()).and_then(Form::symbol) == Some("quote") {
                return;
            }
            for f in items {
                walk(f, namespace, names, own, indexed, u);
            }
        }
        Form::Meta { target, .. } => walk(target, namespace, names, own, indexed, u),
        Form::Prefixed { prefix, inner, .. } => match prefix.as_str() {
            "'" => {}
            "#'" | "#=" => u.impure = true,
            _ => walk(inner, namespace, names, own, indexed, u),
        },
        Form::Atom {
            kind: TokenKind::Symbol,
            text,
            ..
        } => symbol(text, namespace, names, own, indexed, u),
        Form::Atom { .. } => {}
    }
}

fn symbol(
    sym: &str,
    namespace: &str,
    names: &Names,
    own: &BTreeSet<String>,
    indexed: &BTreeSet<String>,
    u: &mut Uses,
) {
    let policy = clojurecraft_clojure::harness_policy();
    let qualified = sym
        .split_once('/')
        .filter(|(ns, name)| !ns.is_empty() && !name.is_empty());
    let Some((ns, name)) = qualified else {
        if policy.denied_core.contains(sym) {
            u.impure = true;
        } else if own.contains(sym) {
            u.calls.insert(format!("{namespace}/{sym}"));
        } else if let Some(lib) = names.referred.get(sym) {
            u.calls.insert(format!("{lib}/{sym}"));
        }
        return;
    };
    if !names.aliases.contains_key(ns) && clojurecraft_clojure::is_class_name(ns) {
        return;
    }
    let target = names.aliases.get(ns).map_or(ns, String::as_str);
    if target == deps::CORE {
        u.impure |= policy.denied_core.contains(name);
    } else if indexed.contains(target) {
        u.calls.insert(format!("{target}/{name}"));
    } else if !deps::is_supported(target) {
        // A namespace without indexed sources: nothing shows that it is pure.
        u.impure = true;
    }
}

/// The text of a definition's chunk: its signature (or a def's value) and docstring, or its
/// source when it has no docstring.
fn chunk_text(text: &str, form: &Form, head: &str, name: &str) -> String {
    let Some(s) = summarize(text, form) else {
        return String::new();
    };
    let mut out = format!("({head} {name}");
    if !s.arglists.is_empty() {
        out.push(' ');
        out.push_str(&s.arglists.join(" "));
    } else if let Some(v) = &s.value {
        out.push(' ');
        out.push_str(v);
    }
    out.push(')');
    match &s.doc {
        Some(doc) => {
            out.push('\n');
            out.push_str(doc.trim_matches('"'));
        }
        None if s.value.is_none() => {
            let source = &text[full_span(form)];
            let source = match source.char_indices().nth(MAX_SOURCE_CHARS) {
                Some((i, _)) => format!("{} …", &source[..i]),
                None => source.to_owned(),
            };
            out.push_str("\nNo docstring; the source:\n");
            out.push_str(&source);
        }
        None => {}
    }
    out
}

/// Indexes `files`. A namespace defined in more than one file is taken from the first.
pub fn index(files: &[SourceFile]) -> ProjectIndex {
    let mut out = ProjectIndex::default();
    let mut parsed: Vec<(&SourceFile, String, Vec<Form>)> = Vec::new();
    for f in files {
        let (Ok(layout), Ok(forms)) = (layout(&f.text), read_all(&f.text)) else {
            out.unreadable.push(f.location.clone());
            continue;
        };
        // Build scripts and other files without a namespace are no library code.
        let Some(ns) = layout.namespace else { continue };
        if out.graph.contains_key(&ns) {
            continue;
        }
        out.graph.insert(ns.clone(), BTreeSet::new());
        if f.library {
            out.library.insert(ns.clone());
        }
        parsed.push((f, ns, forms));
    }
    let indexed: BTreeSet<String> = out.graph.keys().cloned().collect();
    for (f, ns, _) in &parsed {
        let requires = project::file_requires(&f.text)
            .into_iter()
            .filter(|r| indexed.contains(r) && r != ns)
            .collect();
        out.graph.insert(ns.clone(), requires);
    }

    let mut defs: Vec<(Def<'_>, &SourceFile, Uses)> = Vec::new();
    for (f, ns, forms) in &parsed {
        let names = names_of(&f.text);
        let mut found: Vec<Def<'_>> = Vec::new();
        for form in forms {
            let items = form.list().unwrap_or_default();
            let head = items.first().and_then(Form::symbol).unwrap_or_default();
            let name_form = items.get(1);
            let Some(name) = name_form.and_then(Form::symbol) else {
                continue;
            };
            if !matches!(head, "defn" | "defn-" | "def") {
                continue;
            }
            found.push(Def {
                namespace: ns.clone(),
                name: name.to_owned(),
                head: head.to_owned(),
                private: head == "defn-" || name_form.is_some_and(Form::is_private),
                form,
            });
        }
        let own: BTreeSet<String> = found.iter().map(|d| d.name.clone()).collect();
        for d in found {
            let u = uses(d.form, ns, &names, &own, &indexed);
            defs.push((d, f, u));
        }
    }

    // Impure is whatever does something impure itself or calls an impure or unknown definition.
    let known: BTreeSet<String> = defs
        .iter()
        .map(|(d, _, _)| format!("{}/{}", d.namespace, d.name))
        .collect();
    let mut impure: BTreeSet<String> = defs
        .iter()
        .filter(|(_, _, u)| u.impure || u.calls.iter().any(|c| !known.contains(c)))
        .map(|(d, _, _)| format!("{}/{}", d.namespace, d.name))
        .collect();
    loop {
        let more: Vec<String> = defs
            .iter()
            .map(|(d, _, u)| (format!("{}/{}", d.namespace, d.name), u))
            .filter(|(q, u)| !impure.contains(q) && u.calls.iter().any(|c| impure.contains(c)))
            .map(|(q, _)| q)
            .collect();
        if more.is_empty() {
            break;
        }
        impure.extend(more);
    }

    for (d, f, _) in defs.iter().filter(|(d, _, _)| !d.private) {
        let qualified = format!("{}/{}", d.namespace, d.name);
        out.chunks.push(Chunk {
            id: format!("project:{qualified}"),
            kind: ChunkKind::Project,
            source: if f.library {
                SourceKey::Library
            } else {
                SourceKey::Project
            },
            title: qualified.clone(),
            namespace: Some(d.namespace.clone()),
            symbol: Some(d.name.clone()),
            url: f.location.clone(),
            since: None,
            restricted: impure.contains(&qualified),
            mentions: Vec::new(),
            text: chunk_text(&f.text, d.form, &d.head, &d.name),
        });
    }
    out
}

/// The definitions of other namespaces that `code` calls as `alias/name`, qualified with the
/// namespace the alias stands for in `file`'s requirements. Standard-library namespaces and
/// static class members are left out.
pub fn external_calls(file: &str, code: &str) -> Vec<String> {
    let names = names_of(file);
    let policy = clojurecraft_clojure::harness_policy();
    let mut out: Vec<String> = Vec::new();
    for t in clojurecraft_edn::tokenize(code) {
        if t.kind != TokenKind::Symbol {
            continue;
        }
        let Some((ns, name)) = code[t.span]
            .split_once('/')
            .filter(|(a, b)| !a.is_empty() && !b.is_empty())
        else {
            continue;
        };
        let target = names.aliases.get(ns).map_or(ns, String::as_str);
        let call = format!("{target}/{name}");
        if !policy.allowed_classes.contains(ns) && !deps::is_supported(target) && !out.contains(&call) {
            out.push(call);
        }
    }
    out
}

/// `namespace` and every indexed namespace that requires it, directly or through others:
/// requiring any of them from `namespace` would make the requirements circular.
pub fn dependents(graph: &BTreeMap<String, BTreeSet<String>>, namespace: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::from([namespace.to_owned()]);
    loop {
        let more: Vec<String> = graph
            .iter()
            .filter(|(ns, requires)| !out.contains(*ns) && requires.iter().any(|r| out.contains(r)))
            .map(|(ns, _)| ns.clone())
            .collect();
        if more.is_empty() {
            return out;
        }
        out.extend(more);
    }
}

/// The indexed namespaces a function in `namespace` may require: every one that does not
/// depend on `namespace`.
pub fn usable(graph: &BTreeMap<String, BTreeSet<String>>, namespace: &str) -> Vec<String> {
    let blocked = dependents(graph, namespace);
    graph
        .keys()
        .filter(|ns| !blocked.contains(*ns))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(location: &str, text: &str, library: bool) -> SourceFile {
        SourceFile {
            location: location.into(),
            text: text.into(),
            library,
        }
    }

    fn files() -> Vec<SourceFile> {
        vec![
            file(
                "src/shop/util.clj",
                "(ns shop.util (:require [clojure.string :as str]))\n\
                 (def ^:private sep \",\")\n\
                 (defn clean\n  \"Trims and lower-cases a name.\"\n  [s]\n  (str/lower-case (str/trim s)))\n\
                 (defn split-csv [line] (str/split line (re-pattern sep)))\n\
                 (defn- shout [s] (str/upper-case s))\n",
                false,
            ),
            file(
                "src/shop/io.clj",
                "(ns shop.io (:require [clojure.java.io :as io] [shop.util :as u]))\n\
                 (defn read-names \"Reads names from a file.\" [path] (map u/clean (line-seq (io/reader path))))\n\
                 (defn names-of [path] (read-names path))\n\
                 (defn throws [x] (throw (IllegalArgumentException. (str \"bad \" x))))\n",
                false,
            ),
            file(
                "src/shop/orders.clj",
                "(ns shop.orders (:require [shop.util :refer [clean]] [xlib.ids :as ids]))\n\
                 (defn label [o] (clean (:name o)))\n\
                 (defn order-id [o] (ids/make (:id o)))\n\
                 (defn sensor [id] (UUIDv5/nameUUIDFromNamespaceAndString id \"x\"))\n",
                false,
            ),
            file(
                "xlib.jar!/xlib/ids.clj",
                "(ns xlib.ids)\n(defn make \"An id.\" [n] (str \"id-\" n))\n",
                true,
            ),
            file("src/broken.clj", "(ns broken (defn", false),
            file("xlib.jar!/project.clj", "(defproject xlib \"1.0\")", true),
        ]
    }

    fn by_title(i: &ProjectIndex, title: &str) -> Chunk {
        i.chunks
            .iter()
            .find(|c| c.title == title)
            .unwrap_or_else(|| {
                panic!(
                    "{title} in {:?}",
                    i.chunks.iter().map(|c| &c.title).collect::<Vec<_>>()
                )
            })
            .clone()
    }

    #[test]
    fn public_definitions_become_chunks_with_their_docstring_or_source() {
        let i = index(&files());
        let clean = by_title(&i, "shop.util/clean");
        assert_eq!(clean.text, "(defn clean [s])\nTrims and lower-cases a name.");
        assert_eq!(clean.url, "src/shop/util.clj");
        assert_eq!(clean.source, SourceKey::Project);
        let split = by_title(&i, "shop.util/split-csv");
        assert!(
            split
                .text
                .starts_with("(defn split-csv [line])\nNo docstring; the source:\n(defn split-csv"),
            "{}",
            split.text
        );
        assert!(
            !i.chunks
                .iter()
                .any(|c| c.title == "shop.util/shout" || c.title == "shop.util/sep")
        );
        assert_eq!(by_title(&i, "xlib.ids/make").source, SourceKey::Library);
        assert_eq!(i.unreadable, ["src/broken.clj"]);
        assert_eq!(i.library, BTreeSet::from(["xlib.ids".to_owned()]));
    }

    #[test]
    fn impurity_is_found_directly_and_through_calls_across_namespaces() {
        let i = index(&files());
        let restricted = |t: &str| by_title(&i, t).restricted;
        assert!(!restricted("shop.util/clean"));
        assert!(!restricted("shop.util/split-csv"), "calls a private pure def");
        assert!(restricted("shop.io/read-names"), "I/O");
        assert!(restricted("shop.io/names-of"), "calls an impure function");
        assert!(
            !restricted("shop.io/throws"),
            "throwing a standard exception is pure"
        );
        assert!(!restricted("shop.orders/label"), "a referred pure function");
        assert!(!restricted("shop.orders/order-id"), "a pure library function");
        assert!(!restricted("shop.orders/sensor"), "Java interop is allowed");
    }

    #[test]
    fn calls_into_other_namespaces_are_qualified_through_the_aliases() {
        let file = "(ns a (:require [clojure.string :as str] [shop.util :as u] [xlib.ids]))\n";
        let code = "(defn f [x] (u/clean (str/trim (xlib.ids/make (Math/abs x)))) (u/clean x))";
        assert_eq!(external_calls(file, code), ["shop.util/clean", "xlib.ids/make"]);
    }

    #[test]
    fn only_namespaces_that_do_not_depend_on_a_namespace_are_usable_from_it() {
        let i = index(&files());
        assert_eq!(
            i.graph["shop.orders"],
            BTreeSet::from(["shop.util".to_owned(), "xlib.ids".to_owned()])
        );
        assert_eq!(
            dependents(&i.graph, "shop.util"),
            BTreeSet::from(["shop.io".into(), "shop.orders".into(), "shop.util".into()])
        );
        assert_eq!(usable(&i.graph, "shop.util"), ["xlib.ids"]);
        assert_eq!(
            usable(&i.graph, "shop.orders"),
            ["shop.io", "shop.util", "xlib.ids"]
        );
    }
}
