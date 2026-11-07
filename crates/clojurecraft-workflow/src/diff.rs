//! Line differences between two versions of source code, grouped into hunks with surrounding
//! context, for reviewing a proposed replacement before it is applied.

use similar::{ChangeTag, TextDiff};

/// Unchanged lines shown around each change.
pub const CONTEXT_LINES: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Unchanged,
    Removed,
    Added,
}

/// One line of a hunk, with its 1-based number in the old and new text where it appears.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old: Option<usize>,
    pub new: Option<usize>,
    /// The line without its line break.
    pub text: String,
}

/// A run of changed lines with context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub lines: Vec<DiffLine>,
}

impl Hunk {
    /// The `@@ -a,b +c,d @@` header of a unified diff.
    pub fn header(&self) -> String {
        let range = |nums: Vec<usize>| match (nums.first(), nums.len()) {
            (Some(first), n) => format!("{first},{n}"),
            (None, _) => "0,0".to_owned(),
        };
        let old = range(self.lines.iter().filter_map(|l| l.old).collect());
        let new = range(self.lines.iter().filter_map(|l| l.new).collect());
        format!("@@ -{old} +{new} @@")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodeDiff {
    pub hunks: Vec<Hunk>,
    pub added: usize,
    pub removed: usize,
}

impl CodeDiff {
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }

    /// For example "3 lines added, 1 removed", or "no changes".
    pub fn summary(&self) -> String {
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        match (self.added, self.removed) {
            (0, 0) => "no changes".to_owned(),
            (a, 0) => format!("{a} line{} added", plural(a)),
            (0, r) => format!("{r} line{} removed", plural(r)),
            (a, r) => format!("{a} line{} added, {r} removed", plural(a)),
        }
    }

    /// The difference as unified-diff text (without file headers).
    pub fn unified(&self) -> String {
        let mut out = String::new();
        for h in &self.hunks {
            out.push_str(&h.header());
            out.push('\n');
            for l in &h.lines {
                out.push(match l.kind {
                    LineKind::Unchanged => ' ',
                    LineKind::Removed => '-',
                    LineKind::Added => '+',
                });
                out.push_str(&l.text);
                out.push('\n');
            }
        }
        out
    }
}

/// The line differences from `old` to `new`. A missing final line break is not a difference.
pub fn diff(old: &str, new: &str) -> CodeDiff {
    let normalise = |s: &str| {
        let mut s = s.replace("\r\n", "\n");
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s
    };
    let (old, new) = (normalise(old), normalise(new));
    let text = TextDiff::from_lines(old.as_str(), new.as_str());
    let mut out = CodeDiff::default();
    for group in text.grouped_ops(CONTEXT_LINES) {
        let mut lines = Vec::new();
        for op in &group {
            for change in text.iter_changes(op) {
                let kind = match change.tag() {
                    ChangeTag::Equal => LineKind::Unchanged,
                    ChangeTag::Delete => {
                        out.removed += 1;
                        LineKind::Removed
                    }
                    ChangeTag::Insert => {
                        out.added += 1;
                        LineKind::Added
                    }
                };
                lines.push(DiffLine {
                    kind,
                    old: change.old_index().map(|i| i + 1),
                    new: change.new_index().map(|i| i + 1),
                    text: change.value().trim_end_matches('\n').to_owned(),
                });
            }
        }
        if lines.iter().any(|l| l.kind != LineKind::Unchanged) {
            out.hunks.push(Hunk { lines });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_code_has_no_hunks_even_without_a_final_line_break() {
        let d = diff("(ns a)\n(defn f [x] x)", "(ns a)\r\n(defn f [x] x)\n");
        assert!(d.is_empty());
        assert!(d.hunks.is_empty());
        assert_eq!(d.summary(), "no changes");
    }

    #[test]
    fn changes_are_grouped_with_context_and_numbered_on_both_sides() {
        let old: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let new = old
            .replace("line 3\n", "line three\n")
            .replace("line 17\n", "")
            .replace("line 20\n", "line 20\nline 21\n");
        let d = diff(&old, &new);
        assert_eq!((d.added, d.removed), (2, 2));
        assert_eq!(d.summary(), "2 lines added, 2 removed");
        // Changes far apart form separate hunks; close ones (17 and 20) share one.
        assert_eq!(d.hunks.len(), 2);
        let first = &d.hunks[0];
        assert_eq!(first.header(), "@@ -1,6 +1,6 @@");
        let removed = first.lines.iter().find(|l| l.kind == LineKind::Removed).unwrap();
        assert_eq!(
            (removed.old, removed.new, removed.text.as_str()),
            (Some(3), None, "line 3")
        );
        let added = first.lines.iter().find(|l| l.kind == LineKind::Added).unwrap();
        assert_eq!(
            (added.old, added.new, added.text.as_str()),
            (None, Some(3), "line three")
        );
        let unified = d.unified();
        assert!(unified.contains("-line 3\n+line three\n"), "{unified}");
        assert!(unified.contains("-line 17\n"), "{unified}");
        assert!(unified.contains(" line 20\n+line 21\n"), "{unified}");
    }

    #[test]
    fn writing_code_from_nothing_adds_every_line() {
        let d = diff("", "(ns a)\n(defn f [x] x)\n");
        assert_eq!((d.added, d.removed), (2, 0));
        assert_eq!(d.summary(), "2 lines added");
    }
}
