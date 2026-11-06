//! Tokenization. The lexer never fails: every byte of the input belongs to exactly one token, so
//! editors can highlight any text, including incomplete or invalid input.
//!
//! It also recognizes Clojure code syntax (quote, syntax-quote, deref, metadata, anonymous
//! functions, regular expressions, reader conditionals) so the same lexer highlights source code.
//! The EDN parser reports those tokens as errors.

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Delim {
    /// `(` `)`
    Paren,
    /// `[` `]`
    Bracket,
    /// `{` `}`
    Brace,
    /// `#{` `}`
    Set,
}

impl Delim {
    pub fn open(self) -> &'static str {
        match self {
            Self::Paren => "(",
            Self::Bracket => "[",
            Self::Brace => "{",
            Self::Set => "#{",
        }
    }

    pub fn close(self) -> char {
        match self {
            Self::Paren => ')',
            Self::Bracket => ']',
            Self::Brace | Self::Set => '}',
        }
    }

    /// Whether `closer` (a [`Delim`] from a [`TokenKind::Close`]) ends this collection.
    pub fn closed_by(self, closer: Self) -> bool {
        self.close() == closer.close()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenKind {
    /// Spaces, tabs, newlines, and commas.
    Whitespace,
    /// `;` to the end of the line.
    Comment,
    Open(Delim),
    /// The closing character; `Set` never appears here (a set closes with `Brace`).
    Close(Delim),
    String {
        terminated: bool,
    },
    /// `\a`, `\newline`, `A`.
    Char,
    /// Anything starting with a digit, a sign followed by a digit, or `##` (`##Inf`, `##NaN`).
    Number,
    Keyword,
    Symbol,
    Nil,
    Boolean,
    /// `#inst`, `#uuid`, `#my/tag`.
    Tag,
    /// `#:ns` before a map.
    NamespacePrefix,
    /// `#_`
    Discard,
    /// Clojure code syntax that is not EDN: `'`, `` ` ``, `~`, `~@`, `@`, `^`, `#'`, `#(` (as `#`
    /// followed by an ordinary `(`), `#?`, `#?@`, `#=`.
    ReaderMacro,
    /// `#"..."` regular expression literal.
    Regex {
        terminated: bool,
    },
    /// A character that cannot start any token, such as a lone `#`.
    Invalid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte range in the source.
    pub span: Range<usize>,
}

/// Characters that end a symbol, keyword, number, or character literal.
pub(crate) fn is_terminator(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            ',' | '"' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '\\' | '@' | '^' | '`' | '~'
        )
}

pub fn tokenize(src: &str) -> Vec<Token> {
    let mut lexer = Lexer { src, pos: 0 };
    let mut tokens = Vec::new();
    while lexer.pos < src.len() {
        let start = lexer.pos;
        let kind = lexer.next_kind();
        debug_assert!(lexer.pos > start, "lexer must always advance");
        tokens.push(Token {
            kind,
            span: start..lexer.pos,
        });
    }
    tokens
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
}

impl Lexer<'_> {
    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn eat_while(&mut self, f: impl Fn(char) -> bool) {
        while let Some(c) = self.peek() {
            if !f(c) {
                break;
            }
            self.pos += c.len_utf8();
        }
    }

    fn eat_atom(&mut self) {
        self.eat_while(|c| !is_terminator(c));
    }

    /// Consumes a string body after its opening quote. Returns whether it was terminated.
    fn eat_string_body(&mut self) -> bool {
        while let Some(c) = self.bump() {
            match c {
                '"' => return true,
                '\\' => {
                    self.bump();
                }
                _ => {}
            }
        }
        false
    }

    fn next_kind(&mut self) -> TokenKind {
        let start = self.pos;
        let Some(c) = self.bump() else {
            return TokenKind::Invalid;
        };
        match c {
            c if c.is_whitespace() || c == ',' => {
                self.eat_while(|c| c.is_whitespace() || c == ',');
                TokenKind::Whitespace
            }
            ';' => {
                self.eat_while(|c| c != '\n');
                TokenKind::Comment
            }
            '(' => TokenKind::Open(Delim::Paren),
            '[' => TokenKind::Open(Delim::Bracket),
            '{' => TokenKind::Open(Delim::Brace),
            ')' => TokenKind::Close(Delim::Paren),
            ']' => TokenKind::Close(Delim::Bracket),
            '}' => TokenKind::Close(Delim::Brace),
            '"' => TokenKind::String {
                terminated: self.eat_string_body(),
            },
            '\\' => {
                // The first character after the backslash always belongs to the literal, even a
                // delimiter: `\(` is the character `(`.
                if self.bump().is_some() {
                    self.eat_atom();
                    TokenKind::Char
                } else {
                    TokenKind::Invalid
                }
            }
            '\'' | '`' | '@' | '^' => TokenKind::ReaderMacro,
            '~' => {
                if self.peek() == Some('@') {
                    self.bump();
                }
                TokenKind::ReaderMacro
            }
            '#' => self.dispatch(),
            ':' => {
                self.eat_atom();
                TokenKind::Keyword
            }
            _ => {
                self.eat_atom();
                classify_atom(&self.src[start..self.pos])
            }
        }
    }

    fn dispatch(&mut self) -> TokenKind {
        match self.peek() {
            Some('{') => {
                self.bump();
                TokenKind::Open(Delim::Set)
            }
            Some('_') => {
                self.bump();
                TokenKind::Discard
            }
            Some('"') => {
                self.bump();
                TokenKind::Regex {
                    terminated: self.eat_string_body(),
                }
            }
            Some('#') => {
                self.bump();
                self.eat_atom();
                TokenKind::Number
            }
            Some(':') => {
                self.bump();
                self.eat_while(|c| !is_terminator(c));
                TokenKind::NamespacePrefix
            }
            Some('\'') | Some('=') => {
                self.bump();
                TokenKind::ReaderMacro
            }
            Some('?') => {
                self.bump();
                if self.peek() == Some('@') {
                    self.bump();
                }
                TokenKind::ReaderMacro
            }
            // `#(`: the `#` is the macro; the parenthesis is lexed as an ordinary opener.
            Some('(') => TokenKind::ReaderMacro,
            Some(c) if c.is_alphabetic() => {
                self.eat_atom();
                TokenKind::Tag
            }
            _ => TokenKind::Invalid,
        }
    }
}

fn classify_atom(text: &str) -> TokenKind {
    let mut chars = text.chars();
    let first = chars.next();
    let second = chars.next();
    match (first, second) {
        (Some(d), _) if d.is_ascii_digit() => TokenKind::Number,
        (Some('+' | '-'), Some(d)) if d.is_ascii_digit() => TokenKind::Number,
        _ => match text {
            "nil" => TokenKind::Nil,
            "true" | "false" => TokenKind::Boolean,
            _ => TokenKind::Symbol,
        },
    }
}

/// How an editor should color a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Plain,
    Comment,
    /// A delimiter at the given nesting depth (0 for top level), for depth-colored brackets.
    Delimiter(u16),
    String,
    Char,
    Number,
    Keyword,
    Symbol,
    /// `nil`, `true`, `false`.
    Literal,
    Tag,
    ReaderMacro,
    Invalid,
}

/// Classifies every token, assigning delimiter depths. Covers the whole input.
pub fn highlight(src: &str) -> Vec<(Range<usize>, Class)> {
    let mut depth: u16 = 0;
    tokenize(src)
        .into_iter()
        .map(|t| {
            let class = match t.kind {
                TokenKind::Whitespace => Class::Plain,
                TokenKind::Comment => Class::Comment,
                TokenKind::Open(_) => {
                    let d = depth;
                    depth = depth.saturating_add(1);
                    Class::Delimiter(d)
                }
                TokenKind::Close(_) => {
                    depth = depth.saturating_sub(1);
                    Class::Delimiter(depth)
                }
                TokenKind::String { terminated: true } | TokenKind::Regex { terminated: true } => {
                    Class::String
                }
                TokenKind::String { terminated: false } | TokenKind::Regex { terminated: false } => {
                    Class::Invalid
                }
                TokenKind::Char => Class::Char,
                TokenKind::Number => Class::Number,
                TokenKind::Keyword => Class::Keyword,
                TokenKind::Symbol => Class::Symbol,
                TokenKind::Nil | TokenKind::Boolean => Class::Literal,
                TokenKind::Tag | TokenKind::NamespacePrefix => Class::Tag,
                TokenKind::Discard => Class::Comment,
                TokenKind::ReaderMacro => Class::ReaderMacro,
                TokenKind::Invalid => Class::Invalid,
            };
            (t.span, class)
        })
        .collect()
}
