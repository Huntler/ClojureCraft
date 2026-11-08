//! Closing delimiters inferred from indentation, for generated code that cannot be read.
//!
//! Small models indent Clojure reliably but often miscount closing delimiters, especially in long
//! functions. This follows parinfer's indent mode: the closing delimiters at the end of each line
//! (the "paren trail") are dropped and computed again from the indentation of the next line. A
//! line indented to column `x` closes every collection opened at column `x` or further right; the
//! end of the code closes everything still open. A closing delimiter in the middle of a line that
//! does not match the innermost open collection is removed.
//!
//! The result is used only when the original code cannot be read and the repaired code can.
//! Strings, characters, regular expressions, and comments are never changed. Whether the repaired
//! code does the right thing is up to the usual checks.

use clojurecraft_edn::{Delim, Token, TokenKind, tokenize};

use crate::structure::read_all;

/// The code with its closing delimiters inferred from indentation, when `code` cannot be read
/// and the result can. `None` when the code reads already, when a string is never closed, or when
/// inferring the delimiters does not help.
pub fn repair(code: &str) -> Option<String> {
    if read_all(code).is_ok() {
        return None;
    }
    let tokens = tokenize(code);
    if tokens.iter().any(|t| {
        matches!(
            t.kind,
            TokenKind::String { terminated: false }
                | TokenKind::Regex { terminated: false }
                | TokenKind::Invalid
        )
    }) {
        return None;
    }
    let repaired = infer(code, &tokens);
    (repaired != code && read_all(&repaired).is_ok()).then_some(repaired)
}

/// Whether the closing delimiter at `i` ends its line: only closing delimiters, spaces, and a
/// comment follow it before the line break.
fn in_trail(code: &str, tokens: &[Token], i: usize) -> bool {
    for t in &tokens[i + 1..] {
        match t.kind {
            TokenKind::Close(_) => {}
            TokenKind::Whitespace if code[t.span.clone()].contains('\n') => return true,
            TokenKind::Whitespace => {}
            TokenKind::Comment => return true,
            _ => return false,
        }
    }
    true
}

fn infer(code: &str, tokens: &[Token]) -> String {
    let line_start = |at: usize| code[..at].rfind('\n').map_or(0, |i| i + 1);
    let column = |at: usize| code[line_start(at)..at].chars().count();
    let line_of = |at: usize| code[..at].matches('\n').count();

    // Openers still open, with the column they were opened at.
    let mut open: Vec<(Delim, usize)> = Vec::new();
    // Text to insert at a byte offset, and byte ranges to drop.
    let mut inserts: Vec<(usize, String)> = Vec::new();
    let mut drops: Vec<std::ops::Range<usize>> = Vec::new();
    // End of the last code token kept, where inferred closers go.
    let mut last_code_end = 0;
    // The line on which the last code token ended.
    let mut last_line: Option<usize> = None;

    let close_from = |open: &mut Vec<(Delim, usize)>, column: usize| -> String {
        let mut closers = String::new();
        while open.last().is_some_and(|(_, c)| *c >= column) {
            let (d, _) = open.pop().expect("checked above");
            closers.push(d.close());
        }
        closers
    };

    for (i, t) in tokens.iter().enumerate() {
        if matches!(t.kind, TokenKind::Whitespace | TokenKind::Comment) {
            continue;
        }
        // The first code token of a line sets the indentation; a token right after a string
        // that spans lines does not.
        let starts_line = last_line.is_none_or(|l| line_of(t.span.start) > l);
        if starts_line && last_line.is_some() {
            let closers = close_from(&mut open, column(t.span.start));
            if !closers.is_empty() {
                inserts.push((last_code_end, closers));
            }
        }
        match t.kind {
            TokenKind::Open(d) => {
                // The column of the opening character itself (`#{` opens at its `#`).
                open.push((d, column(t.span.start)));
            }
            TokenKind::Close(c) => {
                if in_trail(code, tokens, i) {
                    drops.push(t.span.clone());
                    continue;
                }
                match open.last() {
                    Some((d, _)) if d.closed_by(c) => {
                        open.pop();
                    }
                    _ => {
                        drops.push(t.span.clone());
                        continue;
                    }
                }
            }
            _ => {}
        }
        last_code_end = t.span.end;
        last_line = Some(line_of(t.span.end));
    }
    let closers = close_from(&mut open, 0);
    if !closers.is_empty() {
        inserts.push((last_code_end, closers));
    }

    let mut out = String::with_capacity(code.len() + 16);
    let mut at = 0;
    let mut inserts = inserts.into_iter().peekable();
    for (pos, c) in code.char_indices() {
        while let Some((_, text)) = inserts.next_if(|(p, _)| *p == pos) {
            out.push_str(&text);
        }
        if !drops.iter().any(|d| d.contains(&pos)) {
            out.push(c);
        }
        at = pos + c.len_utf8();
    }
    for (_, text) in inserts.filter(|(p, _)| *p >= at) {
        out.push_str(&text);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_closers_are_inferred_from_the_next_line() {
        let code = "(ns a.core)\n\n(defn- twice [x]\n  (* 2 x)\n\n(defn f [xs]\n  (let [ys (map twice xs)]\n    (vec ys)))\n";
        assert_eq!(
            repair(code).unwrap(),
            "(ns a.core)\n\n(defn- twice [x]\n  (* 2 x))\n\n(defn f [xs]\n  (let [ys (map twice xs)]\n    (vec ys)))\n"
        );
    }

    #[test]
    fn extra_closers_at_the_end_of_a_line_are_dropped() {
        let code = "(ns a.core)\n(defn f [x]\n  (inc x))))\n";
        assert_eq!(repair(code).unwrap(), "(ns a.core)\n(defn f [x]\n  (inc x))\n");
    }

    #[test]
    fn mismatched_closers_are_corrected() {
        let code = "(ns a.core)\n(defn f [m]\n  (let [a (:a m]\n    (inc a))))\n";
        assert_eq!(
            repair(code).unwrap(),
            "(ns a.core)\n(defn f [m]\n  (let [a (:a m)]\n    (inc a)))\n"
        );
    }

    #[test]
    fn strings_comments_and_characters_are_left_alone() {
        let code = "(ns a.core)\n(defn f [s]\n  ;; close ) here\n  (str s \")]}\" \\) \"multi\nline\" s)))\n";
        assert_eq!(
            repair(code).unwrap(),
            "(ns a.core)\n(defn f [s]\n  ;; close ) here\n  (str s \")]}\" \\) \"multi\nline\" s))\n"
        );
    }

    #[test]
    fn readable_or_hopeless_code_is_not_changed() {
        assert_eq!(repair("(ns a)\n(defn f [x] x)\n"), None);
        assert_eq!(repair("(ns a)\n(defn f [x] (str \"x))\n"), None);
    }
}
