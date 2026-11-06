//! Turning documentation sources into retrievable chunks.
//!
//! API documentation is chunked by symbol: one chunk per var or special form, with its arglists,
//! kind, version, and docstring. Reference and guide pages are chunked by section; long sections
//! are split between paragraphs, and code examples always stay in the same chunk as the
//! paragraph that introduces them.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::asciidoc::{self, Block};
use crate::manifest::{ApiSource, Page, PageKind};

/// Chunks longer than this (in characters) are split between paragraphs where possible.
pub const MAX_CHUNK_CHARS: usize = 2400;
/// A paragraph this short that follows a code example is treated as its explanation.
const TRAILING_EXPLANATION_CHARS: usize = 320;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChunkKind {
    /// A function, macro, or var of the standard library.
    Api,
    SpecialForm,
    Reference,
    Guide,
}

impl ChunkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::SpecialForm => "special-form",
            Self::Reference => "reference",
            Self::Guide => "guide",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "api" => Self::Api,
            "special-form" => Self::SpecialForm,
            "reference" => Self::Reference,
            "guide" => Self::Guide,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Api => "API",
            Self::SpecialForm => "Special form",
            Self::Reference => "Reference",
            Self::Guide => "Guide",
        }
    }
}

/// Which documentation source a chunk comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKey {
    Api,
    Site,
}

impl SourceKey {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Site => "site",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    /// Stable identifier, e.g. `api:clojure.core/map` or `page:reference/sequences#_the_seq_library`.
    pub id: String,
    pub kind: ChunkKind,
    pub source: SourceKey,
    /// Human-readable title: the qualified symbol, or the page title and section headings.
    pub title: String,
    pub namespace: Option<String>,
    pub symbol: Option<String>,
    /// Where the documented text is published.
    pub url: String,
    /// The Clojure version that introduced the symbol, when known.
    pub since: Option<String>,
    /// The symbol cannot be used in generated functions (I/O, evaluation, interop, mutation).
    pub restricted: bool,
    /// API symbols the text refers to, as `namespace/name`.
    pub mentions: Vec<String>,
    pub text: String,
}

/// API documentation as printed by the extraction script.
#[derive(Clone, Debug, Deserialize)]
pub struct ApiDocs {
    pub clojure_version: String,
    pub entries: Vec<ApiEntry>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ApiEntry {
    pub ns: String,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub arglists: Vec<String>,
    pub doc: Option<String>,
    pub added: Option<String>,
    pub deprecated: Option<String>,
    #[serde(default)]
    pub dynamic: bool,
}

/// Symbols of `clojure.core` that the evaluation harness refuses in generated code, read from
/// the harness itself so the two lists cannot diverge.
pub fn restricted_core_symbols() -> BTreeSet<String> {
    let src = clojurecraft_clojure::HARNESS_SOURCE;
    let Some(start) = src.find("(def denied-core") else {
        return BTreeSet::new();
    };
    let rest = &src[start..];
    let (Some(open), Some(close)) = (rest.find("#{"), rest.find('}')) else {
        return BTreeSet::new();
    };
    rest[open + 2..close]
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Removes the indentation Clojure docstrings carry on their continuation lines.
pub fn dedent_docstring(doc: &str) -> String {
    let mut lines = doc.lines();
    let first = lines.next().unwrap_or("").trim().to_owned();
    let rest: Vec<&str> = lines.collect();
    let indent = rest
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut out = first;
    for l in rest {
        out.push('\n');
        out.push_str(l.get(indent..).unwrap_or(l.trim_start()).trim_end());
    }
    out.trim().to_owned()
}

fn usage(entry: &ApiEntry) -> Vec<String> {
    entry
        .arglists
        .iter()
        .map(|a| {
            if entry.kind == "special-form" {
                return a.clone();
            }
            let inner = a.trim().trim_start_matches('[').trim_end_matches(']').trim();
            if inner.is_empty() {
                format!("({})", entry.name)
            } else {
                format!("({} {inner})", entry.name)
            }
        })
        .collect()
}

pub fn api_chunks(api: &ApiDocs, source: &ApiSource, restricted_core: &BTreeSet<String>) -> Vec<Chunk> {
    let mut seen = BTreeSet::new();
    api.entries
        .iter()
        .filter(|e| e.doc.as_deref().is_some_and(|d| !d.trim().is_empty()))
        .filter(|e| seen.insert(format!("{}/{}", e.ns, e.name)))
        .map(|e| {
            let special = e.kind == "special-form";
            let what = match e.kind.as_str() {
                "macro" => "Macro",
                "special-form" => "Special form",
                "function" => "Function",
                _ if e.dynamic => "Dynamic var",
                _ => "Var",
            };
            let mut header = format!("{what} in {}", e.ns);
            if let Some(added) = &e.added {
                header.push_str(&format!(", added in Clojure {added}"));
            }
            if let Some(dep) = &e.deprecated {
                header.push_str(&format!(", deprecated since {dep}"));
            }
            header.push('.');
            let restricted = e.ns == "clojure.core" && restricted_core.contains(&e.name);
            if restricted {
                header.push_str(" Not available in ClojureCraft functions.");
            }
            let mut text = usage(e).join("\n");
            text.push('\n');
            text.push_str(&header);
            text.push_str("\n\n");
            text.push_str(&dedent_docstring(e.doc.as_deref().unwrap_or_default()));
            Chunk {
                id: format!("api:{}/{}", e.ns, e.name),
                kind: if special {
                    ChunkKind::SpecialForm
                } else {
                    ChunkKind::Api
                },
                source: SourceKey::Api,
                title: format!("{}/{}", e.ns, e.name),
                namespace: Some(e.ns.clone()),
                symbol: Some(e.name.clone()),
                url: if special {
                    source.special_form_url.clone()
                } else {
                    source.url_for(&e.ns, &e.name)
                },
                since: e.added.clone(),
                restricted,
                mentions: Vec::new(),
                text,
            }
        })
        .collect()
}

fn render(blocks: &[Block]) -> String {
    blocks
        .iter()
        .map(|b| match b {
            Block::Text(t) => t.clone(),
            Block::Code(c) => format!("```\n{c}\n```"),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Groups blocks so that a code example stays with the paragraph before it and with a short
/// explanation after it.
fn units(blocks: &[Block]) -> Vec<Vec<Block>> {
    let mut out: Vec<Vec<Block>> = Vec::new();
    let mut after_code = false;
    for b in blocks {
        let attach = match b {
            Block::Code(_) => out.last().is_some_and(|u| !u.is_empty()),
            Block::Text(t) => after_code && t.len() <= TRAILING_EXPLANATION_CHARS,
        };
        match out.last_mut() {
            Some(unit) if attach => unit.push(b.clone()),
            _ => out.push(vec![b.clone()]),
        }
        after_code = matches!(b, Block::Code(_));
    }
    out
}

fn page_slug(page: &Page) -> String {
    page.path
        .trim_start_matches("content/")
        .trim_end_matches(".adoc")
        .to_owned()
}

/// Chunks one page. A section that fits stays whole; longer sections are split between units.
pub fn page_chunks(page: &Page, source: &str) -> Vec<Chunk> {
    let parsed = asciidoc::parse(source);
    let kind = match page.kind {
        PageKind::Reference => ChunkKind::Reference,
        PageKind::Guide => ChunkKind::Guide,
    };
    let slug = page_slug(page);
    let mut out = Vec::new();
    for section in &parsed.sections {
        let mut title = parsed.title.clone();
        for h in &section.headings {
            title.push_str(" › ");
            title.push_str(h);
        }
        let anchor = section.anchor.clone().unwrap_or_else(|| "_intro".to_owned());
        let url = match &section.anchor {
            Some(a) => format!("{}#{a}", page.url),
            None => page.url.clone(),
        };
        let mut parts: Vec<Vec<Block>> = Vec::new();
        let mut size = 0;
        for unit in units(&section.blocks) {
            let unit_size = render(&unit).len();
            match parts.last_mut() {
                Some(part) if size + unit_size + 2 <= MAX_CHUNK_CHARS => {
                    part.extend(unit);
                    size += unit_size + 2;
                }
                _ => {
                    parts.push(unit);
                    size = unit_size;
                }
            }
        }
        let n = parts.len();
        for (i, part) in parts.into_iter().enumerate() {
            let id = if n == 1 {
                format!("page:{slug}#{anchor}")
            } else {
                format!("page:{slug}#{anchor}/{}", i + 1)
            };
            out.push(Chunk {
                id,
                kind,
                source: SourceKey::Site,
                title: if n == 1 {
                    title.clone()
                } else {
                    format!("{title} ({} of {n})", i + 1)
                },
                namespace: None,
                symbol: None,
                url: url.clone(),
                since: None,
                restricted: false,
                mentions: section.mentions.clone(),
                text: render(&part),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: &str, arglists: &[&str], doc: &str) -> ApiEntry {
        ApiEntry {
            ns: "clojure.core".into(),
            name: name.into(),
            kind: kind.into(),
            arglists: arglists.iter().map(|s| (*s).to_owned()).collect(),
            doc: Some(doc.into()),
            added: Some("1.2".into()),
            deprecated: None,
            dynamic: false,
        }
    }

    fn api_source() -> ApiSource {
        crate::manifest::KnowledgeManifest::bundled()
            .expect("manifest")
            .api
    }

    #[test]
    fn api_entries_become_one_chunk_per_symbol() {
        let api = ApiDocs {
            clojure_version: "1.12.6".into(),
            entries: vec![
                entry(
                    "group-by",
                    "function",
                    &["[f coll]"],
                    "Returns a map of the elements of coll keyed by the result of\n  f on each element.",
                ),
                entry("if", "special-form", &["(if test then else?)"], "Evaluates test."),
                entry("slurp", "function", &["[f & opts]"], "Opens a reader on f."),
                entry("undocumented", "function", &[], " "),
            ],
        };
        let chunks = api_chunks(&api, &api_source(), &restricted_core_symbols());
        assert_eq!(chunks.len(), 3);
        let g = &chunks[0];
        assert_eq!(g.id, "api:clojure.core/group-by");
        assert_eq!(g.symbol.as_deref(), Some("group-by"));
        assert_eq!(g.since.as_deref(), Some("1.2"));
        assert!(
            g.text.starts_with(
                "(group-by f coll)\nFunction in clojure.core, added in Clojure 1.2.\n\nReturns a map of the elements of coll keyed by the result of\nf on each element."
            ),
            "{}",
            g.text
        );
        assert!(g.url.ends_with("clojure.core-api.html#clojure.core/group-by"));
        assert_eq!(chunks[1].kind, ChunkKind::SpecialForm);
        assert!(chunks[1].text.starts_with("(if test then else?)"));
        assert!(chunks[2].restricted, "slurp is refused by the harness");
        assert!(!g.restricted);
    }

    #[test]
    fn the_restricted_list_is_read_from_the_harness() {
        let r = restricted_core_symbols();
        for s in ["eval", "slurp", "read-string", "future", "proxy"] {
            assert!(r.contains(s), "{s}");
        }
        assert!(!r.contains("map"));
    }

    #[test]
    fn long_sections_split_between_paragraphs_and_keep_examples_with_their_explanation() {
        let page = Page {
            path: "content/guides/sample.adoc".into(),
            url: "https://clojure.org/guides/sample".into(),
            kind: PageKind::Guide,
            size: 1,
            sha256: String::new(),
        };
        let para = "Words about transformations. ".repeat(40);
        let mut src = String::from("= Sample\n\n== Big section\n\n");
        for i in 0..6 {
            src.push_str(&format!("{para}{i}\n\nIntro to example {i}:\n\n[source,clojure]\n----\n(example {i})\n----\n\nIt returns {i}.\n\n"));
        }
        let chunks = page_chunks(&page, &src);
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.text.len() <= MAX_CHUNK_CHARS, "{}", c.text.len());
            assert!(c.title.starts_with("Sample › Big section ("));
            assert_eq!(c.url, "https://clojure.org/guides/sample#_big_section");
        }
        for i in 0..6 {
            let holder = chunks
                .iter()
                .find(|c| c.text.contains(&format!("(example {i})")))
                .expect("example present");
            assert!(holder.text.contains(&format!("Intro to example {i}:")));
            assert!(holder.text.contains(&format!("It returns {i}.")));
        }
        let ids: BTreeSet<&str> = chunks.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids.len(), chunks.len());
    }
}
