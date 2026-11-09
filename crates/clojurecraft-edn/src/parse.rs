//! EDN parsing with error recovery. Parsing never fails: it returns every value it could read and
//! a diagnostic for every problem, each with a byte span the editor can point at.
//!
//! The accepted language is what `clojure.edn/read` accepts without custom readers: scalars,
//! lists, vectors, maps (including `#:ns{}`), sets, `#_` discards, and the built-in `#inst` and
//! `#uuid` tags.

use std::collections::HashMap;
use std::ops::Range;

use crate::lex::{Delim, Token, TokenKind, is_terminator, tokenize};

/// Collections nested deeper than this are reported instead of parsed, which bounds recursion.
pub const MAX_DEPTH: usize = 256;
/// Further problems are counted but not reported individually.
pub const MAX_DIAGNOSTICS: usize = 100;

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub span: Range<usize>,
    pub kind: NodeKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NodeKind {
    Nil,
    Boolean(bool),
    Integer,
    Float,
    Ratio,
    String(String),
    Char(char),
    Keyword,
    Symbol,
    List(Vec<Node>),
    Vector(Vec<Node>),
    Map(Vec<(Node, Node)>),
    Set(Vec<Node>),
    Tagged { tag: String, value: Box<Node> },
}

impl Node {
    /// The node's source text.
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.span.clone()]
    }

    /// A short name for the kind of value, for summaries and inspectors.
    pub fn type_name(&self) -> &'static str {
        match &self.kind {
            NodeKind::Nil => "nil",
            NodeKind::Boolean(_) => "boolean",
            NodeKind::Integer => "integer",
            NodeKind::Float => "decimal",
            NodeKind::Ratio => "ratio",
            NodeKind::String(_) => "string",
            NodeKind::Char(_) => "character",
            NodeKind::Keyword => "keyword",
            NodeKind::Symbol => "symbol",
            NodeKind::List(_) => "list",
            NodeKind::Vector(_) => "vector",
            NodeKind::Map(_) => "map",
            NodeKind::Set(_) => "set",
            NodeKind::Tagged { .. } => "tagged value",
        }
    }

    /// Number of direct children (entries for a map), or `None` for scalars.
    pub fn child_count(&self) -> Option<usize> {
        match &self.kind {
            NodeKind::List(v) | NodeKind::Vector(v) | NodeKind::Set(v) => Some(v.len()),
            NodeKind::Map(e) => Some(e.len()),
            _ => None,
        }
    }

    /// One-line description such as `map · 3 entries`.
    pub fn summary(&self) -> String {
        match (self.child_count(), &self.kind) {
            (Some(n), NodeKind::Map(_)) => format!("map · {n} {}", plural(n, "entry", "entries")),
            (Some(n), _) => format!("{} · {n} {}", self.type_name(), plural(n, "item", "items")),
            (None, NodeKind::Tagged { tag, .. }) => format!("#{tag}"),
            (None, _) => self.type_name().to_owned(),
        }
    }
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub span: Range<usize>,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Parsed {
    /// Top-level values in order.
    pub values: Vec<Node>,
    pub diagnostics: Vec<Diagnostic>,
    /// Problems beyond [`MAX_DIAGNOSTICS`] that were not recorded individually.
    pub suppressed: usize,
    /// Byte length of the parsed text, so callers can tell whether spans still apply to it.
    pub source_len: usize,
}

impl Parsed {
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.severity == Severity::Error)
    }

    pub fn error_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count()
            + self.suppressed
    }
}

/// Parses any number of top-level values.
pub fn parse(src: &str) -> Parsed {
    let tokens: Vec<Token> = tokenize(src)
        .into_iter()
        .filter(|t| !matches!(t.kind, TokenKind::Whitespace | TokenKind::Comment))
        .collect();
    let mut p = Parser {
        src,
        tokens,
        pos: 0,
        out: Parsed::default(),
    };
    let mut values = Vec::new();
    while p.pos < p.tokens.len() {
        if let Some(v) = p.form(&[]) {
            values.push(v);
        }
    }
    p.out.values = values;
    p.out.source_len = src.len();
    p.out
}

/// The exception class an expected value names when it is `#throws Class`: the call is expected
/// to throw rather than return.
pub fn expected_throws(src: &str) -> Option<String> {
    let parsed = parse_value(src);
    match parsed.values.first().map(|v| &v.kind) {
        Some(NodeKind::Tagged { tag, value }) if tag == "throws" && !parsed.has_errors() => {
            Some(value.text(src).to_owned())
        }
        _ => None,
    }
}

/// Parses text that must hold exactly one value (an example input or expected output). An empty
/// text has no value and no diagnostics; more than one value is an error.
pub fn parse_value(src: &str) -> Parsed {
    let mut parsed = parse(src);
    // With other errors present, extra values are usually a consequence of them (for example a
    // closing bracket in the wrong place), so only report them on their own.
    if parsed.values.len() > 1 && !parsed.has_errors() {
        let extra = parsed.values[1].span.start..parsed.values[parsed.values.len() - 1].span.end;
        let n = parsed.values.len();
        push(
            &mut parsed,
            Severity::Error,
            extra,
            format!("expected a single value but found {n}; wrap them in a vector `[…]` or a list"),
        );
    }
    parsed
}

fn push(out: &mut Parsed, severity: Severity, span: Range<usize>, message: String) {
    if out.diagnostics.len() >= MAX_DIAGNOSTICS {
        out.suppressed += 1;
    } else {
        out.diagnostics.push(Diagnostic {
            severity,
            span,
            message,
        });
    }
}

struct Parser<'a> {
    src: &'a str,
    tokens: Vec<Token>,
    pos: usize,
    out: Parsed,
}

impl Parser<'_> {
    fn error(&mut self, span: Range<usize>, message: impl Into<String>) {
        push(&mut self.out, Severity::Error, span, message.into());
    }

    fn text(&self, span: &Range<usize>) -> &str {
        &self.src[span.clone()]
    }

    /// Reads one form. `open` lists the delimiters of the enclosing collections, innermost last.
    /// Returns `None` when the token produced no value (it was skipped with a diagnostic, or it
    /// was a closer this form must leave to an enclosing collection).
    fn form(&mut self, open: &[(Delim, Range<usize>)]) -> Option<Node> {
        let token = self.tokens.get(self.pos)?.clone();
        self.pos += 1;
        let span = token.span.clone();
        let kind = match token.kind {
            TokenKind::Open(delim) => return self.collection(delim, span, open),
            TokenKind::Close(delim) => {
                if open.iter().any(|(d, _)| d.closed_by(delim)) {
                    // Leave it to the enclosing collection that it closes.
                    self.pos -= 1;
                } else {
                    let t = self.text(&span).to_owned();
                    self.error(span, format!("unexpected `{t}` with no matching opening bracket"));
                }
                return None;
            }
            TokenKind::String { terminated } => {
                if !terminated {
                    self.error(span, "string is never closed: add a `\"`");
                    return None;
                }
                let raw = &self.src[span.start + 1..span.end - 1];
                match unescape(raw) {
                    Ok(s) => NodeKind::String(s),
                    Err((offset, message)) => {
                        let at = span.start + 1 + offset;
                        self.error(at..(at + 2).min(span.end - 1), message);
                        NodeKind::String(raw.to_owned())
                    }
                }
            }
            TokenKind::Char => match char_literal(self.text(&span)) {
                Some(c) => NodeKind::Char(c),
                None => {
                    let t = self.text(&span).to_owned();
                    self.error(span, format!("`{t}` is not a valid character literal"));
                    return None;
                }
            },
            TokenKind::Number => match number(self.text(&span)) {
                Some(kind) => kind,
                None => {
                    let t = self.text(&span).to_owned();
                    self.error(span, format!("`{t}` is not a valid number"));
                    return None;
                }
            },
            TokenKind::Keyword => {
                if let Err(message) = keyword(self.text(&span)) {
                    self.error(span, message);
                    return None;
                }
                NodeKind::Keyword
            }
            TokenKind::Symbol => {
                if let Err(message) = symbol(self.text(&span)) {
                    self.error(span, message);
                    return None;
                }
                NodeKind::Symbol
            }
            TokenKind::Nil => NodeKind::Nil,
            TokenKind::Boolean => NodeKind::Boolean(self.text(&span) == "true"),
            TokenKind::Tag => return self.tagged(span, open),
            TokenKind::NamespacePrefix => return self.namespaced_map(span, open),
            TokenKind::Discard => {
                match self.tokens.get(self.pos).map(|t| t.kind) {
                    None | Some(TokenKind::Close(_)) => {
                        self.error(span, "`#_` must be followed by the value to ignore");
                    }
                    Some(_) => {
                        self.next_value(open);
                    }
                }
                return None;
            }
            TokenKind::ReaderMacro => {
                let t = self.text(&span).to_owned();
                let what = match t.as_str() {
                    "'" => "quote (`'`)",
                    "`" => "syntax quote (`` ` ``)",
                    "~" | "~@" => "unquote (`~`)",
                    "@" => "deref (`@`)",
                    "^" => "metadata (`^`)",
                    "#'" => "var quote (`#'`)",
                    "#" => "anonymous function (`#(…)`)",
                    "#=" => "read-eval (`#=`)",
                    _ => "a reader conditional (`#?`)",
                };
                self.error(
                    span,
                    format!("{what} is Clojure code syntax and is not allowed in EDN data"),
                );
                // Skip the form it applies to, so one mistake gives one diagnostic.
                let mut quiet = Parsed::default();
                std::mem::swap(&mut quiet, &mut self.out);
                self.form(open);
                std::mem::swap(&mut quiet, &mut self.out);
                return None;
            }
            TokenKind::Regex { .. } => {
                self.error(
                    span,
                    "regular expressions (`#\"…\"`) are Clojure code syntax and are not allowed in EDN data",
                );
                return None;
            }
            TokenKind::Invalid | TokenKind::Whitespace | TokenKind::Comment => {
                let t = self.text(&span).to_owned();
                self.error(span, format!("unexpected `{t}`"));
                return None;
            }
        };
        Some(Node { span, kind })
    }

    fn collection(
        &mut self,
        delim: Delim,
        open_span: Range<usize>,
        outer: &[(Delim, Range<usize>)],
    ) -> Option<Node> {
        let (items, end) = self.collection_items(delim, open_span.clone(), outer)?;
        let span = open_span.start..end;
        let kind = match delim {
            Delim::Paren => NodeKind::List(items),
            Delim::Bracket => NodeKind::Vector(items),
            Delim::Set => NodeKind::Set(self.check_set(items)),
            Delim::Brace => NodeKind::Map(self.pair_entries(items, None)),
        };
        Some(Node { span, kind })
    }

    /// Reads the items of a collection whose opener was just consumed, returning them with the
    /// end offset of the collection.
    fn collection_items(
        &mut self,
        delim: Delim,
        open_span: Range<usize>,
        outer: &[(Delim, Range<usize>)],
    ) -> Option<(Vec<Node>, usize)> {
        if outer.len() >= MAX_DEPTH {
            self.error(
                open_span.clone(),
                format!("data is nested more than {MAX_DEPTH} levels deep"),
            );
            self.skip_collection();
            return None;
        }
        let mut stack = outer.to_vec();
        stack.push((delim, open_span.clone()));
        let mut items = Vec::new();
        let end = loop {
            let Some(token) = self.tokens.get(self.pos).cloned() else {
                let opener = delim.open();
                self.error(
                    open_span.clone(),
                    format!("`{opener}` is never closed: add a matching `{}`", delim.close()),
                );
                break self.src.len();
            };
            if let TokenKind::Close(closer) = token.kind {
                if delim.closed_by(closer) {
                    self.pos += 1;
                    break token.span.end;
                }
                if outer.iter().any(|(d, _)| d.closed_by(closer)) {
                    // The closer belongs to an enclosing collection: this one was never closed.
                    let found = self.text(&token.span).to_owned();
                    self.error(
                        token.span.clone(),
                        format!(
                            "expected `{}` to close `{}` but found `{found}`",
                            delim.close(),
                            delim.open()
                        ),
                    );
                    break token.span.start;
                }
            }
            if let Some(node) = self.form(&stack) {
                items.push(node);
            }
        };
        Some((items, end))
    }

    fn skip_collection(&mut self) {
        let mut depth = 1usize;
        while let Some(t) = self.tokens.get(self.pos) {
            self.pos += 1;
            match t.kind {
                TokenKind::Open(_) => depth += 1,
                TokenKind::Close(_) => {
                    depth -= 1;
                    if depth == 0 {
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    fn check_set(&mut self, items: Vec<Node>) -> Vec<Node> {
        let mut seen: HashMap<String, Range<usize>> = HashMap::new();
        let mut kept = Vec::with_capacity(items.len());
        for item in items {
            let key = canonical(&item, self.src, None);
            if let Some(first) = seen.get(&key) {
                let t = abbreviate(item.text(self.src));
                let (line, col) = line_col(self.src, first.start);
                self.error(
                    item.span.clone(),
                    format!("duplicate set element `{t}` (also at line {line}, column {col})"),
                );
            } else {
                seen.insert(key, item.span.clone());
                kept.push(item);
            }
        }
        kept
    }

    /// Pairs up map items, reporting a missing value and duplicate keys. `namespace` applies to
    /// unqualified keyword keys of a `#:ns{}` map.
    fn pair_entries(&mut self, items: Vec<Node>, namespace: Option<&str>) -> Vec<(Node, Node)> {
        let mut entries = Vec::with_capacity(items.len() / 2);
        let mut seen: HashMap<String, Range<usize>> = HashMap::new();
        let mut it = items.into_iter();
        while let Some(key) = it.next() {
            let Some(value) = it.next() else {
                let t = abbreviate(key.text(self.src));
                self.error(
                    key.span.clone(),
                    format!("map key `{t}` has no value (a map needs an even number of forms)"),
                );
                break;
            };
            let canon = canonical(&key, self.src, namespace);
            if let Some(first) = seen.get(&canon) {
                let t = abbreviate(key.text(self.src));
                let (line, col) = line_col(self.src, first.start);
                self.error(
                    key.span.clone(),
                    format!("duplicate map key `{t}` (also at line {line}, column {col})"),
                );
                continue;
            }
            seen.insert(canon, key.span.clone());
            entries.push((key, value));
        }
        entries
    }

    fn tagged(&mut self, tag_span: Range<usize>, open: &[(Delim, Range<usize>)]) -> Option<Node> {
        let tag = self.src[tag_span.start + 1..tag_span.end].to_owned();
        let value = self.next_value(open);
        let Some(value) = value else {
            self.error(tag_span, format!("`#{tag}` must be followed by a value"));
            return None;
        };
        match tag.as_str() {
            "inst" => match &value.kind {
                NodeKind::String(s) if is_instant(s) => {}
                _ => self.error(
                    value.span.clone(),
                    "`#inst` needs an RFC 3339 timestamp string, for example \"2024-05-01T12:00:00Z\"",
                ),
            },
            "uuid" => match &value.kind {
                NodeKind::String(s) if is_uuid(s) => {}
                _ => self.error(
                    value.span.clone(),
                    "`#uuid` needs a string such as \"3b2a9f30-6f0e-4bd8-9e64-0c6f1c1b4a52\"",
                ),
            },
            // java.time values, as the evaluation harness prints and reads them.
            "time/duration" | "time/date" | "time/date-time" | "time/time" => match &value.kind {
                NodeKind::String(_) => {}
                _ => self.error(
                    value.span.clone(),
                    format!("`#{tag}` needs an ISO-8601 string, for example #time/duration \"PT30M\""),
                ),
            },
            // An expected outcome: the call throws an exception of this class.
            "throws" => match &value.kind {
                NodeKind::Symbol => {}
                _ => self.error(
                    value.span.clone(),
                    "`#throws` needs an exception class name, for example #throws IllegalArgumentException",
                ),
            },
            _ => self.error(
                tag_span.clone(),
                format!(
                    "unknown tag `#{tag}`: only `#inst`, `#uuid`, `#time/duration`, `#time/date`, \
                     `#time/date-time`, `#time/time`, and `#throws` can be read"
                ),
            ),
        }
        Some(Node {
            span: tag_span.start..value.span.end,
            kind: NodeKind::Tagged {
                tag,
                value: Box::new(value),
            },
        })
    }

    /// The next value, skipping discarded forms; `None` at a closer or the end of input.
    fn next_value(&mut self, open: &[(Delim, Range<usize>)]) -> Option<Node> {
        loop {
            match self.tokens.get(self.pos)?.kind {
                TokenKind::Close(_) => return None,
                _ => {
                    if let Some(v) = self.form(open) {
                        return Some(v);
                    }
                }
            }
        }
    }

    fn namespaced_map(&mut self, prefix: Range<usize>, open: &[(Delim, Range<usize>)]) -> Option<Node> {
        let ns = self.src[prefix.start + 2..prefix.end].to_owned();
        let next_is_map = matches!(
            self.tokens.get(self.pos).map(|t| t.kind),
            Some(TokenKind::Open(Delim::Brace))
        );
        if ns.is_empty() || ns.starts_with(':') {
            self.error(
                prefix.clone(),
                "a namespaced map needs an explicit namespace, for example `#:user{:id 1}`",
            );
        } else if !next_is_map {
            self.error(
                prefix.clone(),
                format!("`#:{ns}` must be followed by a map `{{…}}`"),
            );
            return None;
        }
        if !next_is_map {
            return None;
        }
        let open_span = self.tokens[self.pos].span.clone();
        self.pos += 1;
        let (items, end) = self.collection_items(Delim::Brace, open_span, open)?;
        let entries = self.pair_entries(items, Some(&ns));
        Some(Node {
            span: prefix.start..end,
            kind: NodeKind::Map(entries),
        })
    }
}

/// Shortens source text for messages.
fn abbreviate(s: &str) -> String {
    const MAX: usize = 40;
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MAX {
        one_line
    } else {
        let mut t: String = one_line.chars().take(MAX - 1).collect();
        t.push('…');
        t
    }
}

/// A key that is equal for values Clojure considers equal (`1` and `1N`, `[1]` and `(1)`), used
/// to detect duplicate map keys and set elements.
fn canonical(node: &Node, src: &str, namespace: Option<&str>) -> String {
    match &node.kind {
        NodeKind::Nil => "nil".into(),
        NodeKind::Boolean(b) => b.to_string(),
        NodeKind::Integer => match integer_value(node.text(src)) {
            Some(v) => format!("i{v}"),
            None => format!("I{}", node.text(src).trim_end_matches('N')),
        },
        NodeKind::Float => format!("f{}", node.text(src)),
        NodeKind::Ratio => format!("r{}", node.text(src)),
        NodeKind::String(s) => format!("s{s:?}"),
        NodeKind::Char(c) => format!("c{c}"),
        NodeKind::Keyword => {
            let t = node.text(src);
            match namespace {
                Some(ns) if !t.contains('/') => format!("k:{ns}/{}", &t[1..]),
                Some(_) if t.starts_with(":_/") => format!("k:{}", &t[3..]),
                _ => format!("k{t}"),
            }
        }
        NodeKind::Symbol => format!("y{}", node.text(src)),
        NodeKind::List(v) | NodeKind::Vector(v) => {
            let inner: Vec<String> = v.iter().map(|n| canonical(n, src, None)).collect();
            format!("[{}]", inner.join(" "))
        }
        NodeKind::Set(v) => {
            let mut inner: Vec<String> = v.iter().map(|n| canonical(n, src, None)).collect();
            inner.sort();
            format!("#{{{}}}", inner.join(" "))
        }
        NodeKind::Map(e) => {
            let mut inner: Vec<String> = e
                .iter()
                .map(|(k, v)| format!("{} {}", canonical(k, src, None), canonical(v, src, None)))
                .collect();
            inner.sort();
            format!("{{{}}}", inner.join(" "))
        }
        NodeKind::Tagged { tag, value } => format!("#{tag} {}", canonical(value, src, None)),
    }
}

/// 1-based line and column (in characters) of a byte offset.
pub fn line_col(src: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(src.len());
    let before = &src[..floor_char_boundary(src, offset)];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let col = before[line_start..].chars().count() + 1;
    (line, col)
}

/// Character index of a byte offset (editors position cursors by character).
pub fn char_index(src: &str, offset: usize) -> usize {
    src[..floor_char_boundary(src, offset.min(src.len()))]
        .chars()
        .count()
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn unescape(raw: &str) -> Result<String, (usize, String)> {
    let mut out = String::with_capacity(raw.len());
    let mut it = raw.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some((_, e)) = it.next() else {
            return Err((i, "a string cannot end with a lone `\\`".into()));
        };
        match e {
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'n' => out.push('\n'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            'u' => {
                let hex: String = (0..4).filter_map(|_| it.next().map(|(_, h)| h)).collect();
                match u32::from_str_radix(&hex, 16)
                    .ok()
                    .filter(|_| hex.len() == 4)
                    .and_then(char::from_u32)
                {
                    Some(ch) => out.push(ch),
                    None => {
                        return Err((
                            i,
                            format!("`\\u{hex}` is not a valid unicode escape; use four hex digits"),
                        ));
                    }
                }
            }
            '0'..='7' => {
                let mut digits = String::from(e);
                while digits.len() < 3 {
                    match it.peek() {
                        Some(&(_, d @ '0'..='7')) => {
                            digits.push(d);
                            it.next();
                        }
                        _ => break,
                    }
                }
                match u32::from_str_radix(&digits, 8)
                    .ok()
                    .filter(|v| *v <= 0o377)
                    .and_then(char::from_u32)
                {
                    Some(ch) => out.push(ch),
                    None => return Err((i, format!("`\\{digits}` is not a valid octal escape"))),
                }
            }
            other => {
                return Err((
                    i,
                    format!("`\\{other}` is not a valid escape; use `\\\\` for a backslash"),
                ));
            }
        }
    }
    Ok(out)
}

fn char_literal(text: &str) -> Option<char> {
    let body = text.strip_prefix('\\')?;
    let mut chars = body.chars();
    let first = chars.next()?;
    if chars.next().is_none() {
        return Some(first);
    }
    match body {
        "newline" => Some('\n'),
        "space" => Some(' '),
        "tab" => Some('\t'),
        "return" => Some('\r'),
        "backspace" => Some('\u{8}'),
        "formfeed" => Some('\u{c}'),
        _ => {
            if let Some(hex) = body.strip_prefix('u').filter(|h| h.len() == 4) {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else if let Some(oct) = body.strip_prefix('o').filter(|o| (1..=3).contains(&o.len())) {
                u32::from_str_radix(oct, 8)
                    .ok()
                    .filter(|v| *v <= 0o377)
                    .and_then(char::from_u32)
            } else {
                None
            }
        }
    }
}

fn digits(s: &str, radix: u32) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_digit(radix))
}

/// Classifies number syntax accepted by the Clojure reader.
fn number(text: &str) -> Option<NodeKind> {
    if matches!(text, "##Inf" | "##-Inf" | "##NaN") {
        return Some(NodeKind::Float);
    }
    let body = text.strip_prefix(['+', '-']).unwrap_or(text);
    if let Some((n, d)) = body.split_once('/') {
        return (digits(n, 10) && digits(d, 10)).then_some(NodeKind::Ratio);
    }
    if integer_syntax(body) {
        return Some(NodeKind::Integer);
    }
    let body = body.strip_suffix('M').unwrap_or(body);
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let (whole, frac) = match mantissa.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (mantissa, None),
    };
    let ok = digits(whole, 10)
        && frac.is_none_or(|f| f.is_empty() || digits(f, 10))
        && exponent.is_none_or(|e| digits(e.strip_prefix(['+', '-']).unwrap_or(e), 10))
        && (frac.is_some() || exponent.is_some() || text.ends_with('M'));
    ok.then_some(NodeKind::Float)
}

fn integer_syntax(body: &str) -> bool {
    let body = body.strip_suffix('N').unwrap_or(body);
    if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        return digits(hex, 16);
    }
    if let Some((radix, value)) = body.split_once(['r', 'R']) {
        return radix
            .parse::<u32>()
            .ok()
            .filter(|r| (2..=36).contains(r))
            .is_some_and(|r| digits(value, r));
    }
    if body.len() > 1 && body.starts_with('0') {
        return digits(&body[1..], 8);
    }
    digits(body, 10)
}

fn integer_value(text: &str) -> Option<i128> {
    let (neg, body) = match text.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let body = body.strip_suffix('N').unwrap_or(body);
    let v = if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        i128::from_str_radix(hex, 16).ok()?
    } else if let Some((radix, value)) = body.split_once(['r', 'R']) {
        i128::from_str_radix(value, radix.parse().ok()?).ok()?
    } else if body.len() > 1 && body.starts_with('0') {
        i128::from_str_radix(&body[1..], 8).ok()?
    } else {
        body.parse().ok()?
    };
    Some(if neg { -v } else { v })
}

fn keyword(text: &str) -> Result<(), String> {
    if text.starts_with("::") {
        return Err(format!(
            "auto-resolved keywords such as `{text}` are not valid EDN; write the namespace explicitly"
        ));
    }
    let name = &text[1..];
    if name.is_empty() {
        return Err("`:` must be followed by a keyword name, for example `:id`".into());
    }
    symbol_parts(name).map_err(|m| format!("`{text}` is not a valid keyword: {m}"))
}

fn symbol(text: &str) -> Result<(), String> {
    if text == "/" {
        return Ok(());
    }
    symbol_parts(text).map_err(|m| format!("`{text}` is not a valid symbol: {m}"))
}

fn symbol_parts(name: &str) -> Result<(), String> {
    if let Some(bad) = name.chars().find(|&c| is_terminator(c)) {
        return Err(format!("it contains `{bad}`"));
    }
    match name.split_once('/') {
        Some((ns, local)) => {
            if ns.is_empty() || local.is_empty() || local.contains('/') {
                return Err("a namespace and name must be separated by a single `/`".into());
            }
            Ok(())
        }
        None => {
            if name.ends_with(':') || name.contains("::") {
                return Err("it cannot end with `:` or contain `::`".into());
            }
            Ok(())
        }
    }
}

fn is_instant(s: &str) -> bool {
    let b = s.as_bytes();
    let num = |r: Range<usize>| b.get(r.clone()).is_some_and(|x| x.iter().all(u8::is_ascii_digit));
    if !num(0..4) {
        return false;
    }
    if b.len() == 4 {
        return true;
    }
    // Optional parts must appear in order: -MM, -DD, Thh:mm, :ss, .fraction, offset.
    let mut i = 4;
    for (sep, len) in [(b'-', 2), (b'-', 2), (b'T', 2), (b':', 2), (b':', 2)] {
        if i == b.len() {
            return true;
        }
        if b[i] != sep || !num(i + 1..i + 1 + len) {
            break;
        }
        i += 1 + len;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    match &b[i..] {
        [] | [b'Z'] => true,
        [b'+' | b'-', ..] => b.len() - i == 6 && num(i + 1..i + 3) && b[i + 3] == b':' && num(i + 4..i + 6),
        _ => false,
    }
}

fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(p, n)| p.len() == n && digits(p, 16))
}
