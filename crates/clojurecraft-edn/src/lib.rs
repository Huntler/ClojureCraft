//! EDN support for editors: a lexer that classifies any text for syntax highlighting, and a
//! recovering parser that reports positioned diagnostics and builds a value tree for inspection.
//!
//! Source text is never modified or re-serialised; every result refers back to it by byte span.

pub mod lex;
pub mod parse;

pub use lex::{Class, Delim, Token, TokenKind, highlight, tokenize};
pub use parse::{
    Diagnostic, MAX_DEPTH, Node, NodeKind, Parsed, Severity, char_index, line_col, parse, parse_value,
};
