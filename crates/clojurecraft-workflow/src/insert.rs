//! Text inserted into source code for an instrumented copy: probes wrapped around forms and
//! probes placed after a form (a step of a threading macro). Several instrumentations can be
//! combined in one copy; at one offset, wraps ending there close first (innermost first), then
//! forms placed after come, then wraps starting there open (outermost first), so the probes
//! always nest. Nothing contains a line break, so line numbers stay the same.

use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Insertion {
    /// `open` before the form at `span`, `close` after it.
    Wrap {
        span: Range<usize>,
        open: String,
        close: String,
    },
    /// `text` right after offset `at`.
    After { at: usize, text: String },
}

/// `source` with every insertion made.
pub(crate) fn apply(source: &str, insertions: &[Insertion]) -> String {
    // (offset, order at that offset, insertion index, text)
    let mut pieces: Vec<(usize, (u8, i64), usize, &str)> = Vec::new();
    for (i, ins) in insertions.iter().enumerate() {
        match ins {
            Insertion::Wrap { span, open, close } => {
                let len = i64::try_from(span.len()).unwrap_or(i64::MAX);
                // Closing: shorter (inner) spans first; opening: longer (outer) spans first.
                pieces.push((span.end, (0, len), usize::MAX - i, close));
                pieces.push((span.start, (2, -len), i, open));
            }
            Insertion::After { at, text } => pieces.push((*at, (1, 0), i, text)),
        }
    }
    pieces.sort_by_key(|(at, order, i, _)| (*at, *order, *i));
    let mut out = String::with_capacity(source.len() + pieces.iter().map(|p| p.3.len()).sum::<usize>());
    let mut from = 0;
    for (at, _, _, text) in pieces {
        out.push_str(&source[from..at]);
        out.push_str(text);
        from = at;
    }
    out.push_str(&source[from..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(span: Range<usize>, name: &str) -> Insertion {
        Insertion::Wrap {
            span,
            open: format!("({name} "),
            close: ")".into(),
        }
    }

    #[test]
    fn probes_nest_at_shared_offsets() {
        //          0123456789
        let src = "(f (g x))";
        let out = apply(
            src,
            &[
                wrap(3..8, "inner"),
                wrap(0..9, "outer"),
                wrap(3..8, "same"),
                Insertion::After {
                    at: 8,
                    text: " (p! 1)".into(),
                },
            ],
        );
        assert_eq!(out, "(outer (f (inner (same (g x))) (p! 1)))");
        assert_eq!(
            apply("(a)(b)", &[wrap(0..3, "p"), wrap(3..6, "q")]),
            "(p (a))(q (b))"
        );
    }
}
