//! Finding code the model quotes (for example in a step explanation) in the actual source.
//!
//! Quotes rarely match byte for byte: indentation and line breaks differ, and long passages are
//! shortened with an ellipsis. Matching therefore ignores whitespace, and a quote that is not
//! found whole is located by its first and last lines.

use clojurecraft_domain::LineRange;

/// Whitespace-free text with, for each kept character, its byte offset in the original.
fn squeeze(text: &str) -> (String, Vec<usize>) {
    let mut out = String::new();
    let mut offsets = Vec::new();
    for (i, c) in text.char_indices() {
        if !c.is_whitespace() && c != ',' {
            out.push(c);
            offsets.extend(std::iter::repeat_n(i, c.len_utf8()));
        }
    }
    (out, offsets)
}

fn line_of(source: &str, offset: usize) -> u32 {
    u32::try_from(source[..offset.min(source.len())].matches('\n').count() + 1).unwrap_or(u32::MAX)
}

/// The byte range of `needle` in `source`, ignoring whitespace; `None` unless it occurs exactly
/// once (an ambiguous quote cannot be placed).
fn find_once(source: &str, needle: &str) -> Option<(usize, usize)> {
    let (hay, offsets) = squeeze(source);
    let (needle, _) = squeeze(needle);
    if needle.len() < 3 {
        return None;
    }
    let start = hay.find(&needle)?;
    if hay[start + 1..].contains(&needle) {
        return None;
    }
    let end = start + needle.len() - 1;
    Some((offsets[start], offsets[end]))
}

/// Lines of `source` that `excerpt` quotes, or `None` when it cannot be placed reliably.
pub fn locate(source: &str, excerpt: &str) -> Option<LineRange> {
    let range = |(a, b): (usize, usize)| LineRange {
        start: line_of(source, a),
        end: line_of(source, b),
    };
    if let Some(found) = find_once(source, excerpt) {
        return Some(range(found));
    }
    let pieces: Vec<&str> = excerpt
        .split(['…', '\n'])
        .flat_map(|p| p.split("..."))
        .map(str::trim)
        .filter(|p| p.len() >= 3)
        .collect();
    let (first, last) = (pieces.first()?, pieces.last()?);
    let (a, _) = find_once(source, first)?;
    let (_, b) = find_once(source, last)?;
    (a <= b).then(|| range((a, b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "(ns a.core)\n\n(defn totals\n  [orders]\n  (->> orders\n       (group-by :customer)\n       (map (fn [[k vs]] [k (reduce + (map :amount vs))]))\n       (into {})))\n";

    #[test]
    fn quotes_are_found_regardless_of_layout() {
        assert_eq!(
            locate(SRC, "(group-by :customer)"),
            Some(LineRange { start: 6, end: 6 })
        );
        assert_eq!(
            locate(
                SRC,
                "(map (fn [[k vs]]\n [k (reduce + (map :amount vs))]))\n(into {})"
            ),
            Some(LineRange { start: 7, end: 8 })
        );
    }

    #[test]
    fn shortened_quotes_are_placed_by_their_ends() {
        assert_eq!(
            locate(SRC, "(->> orders ... (into {}))"),
            Some(LineRange { start: 5, end: 8 })
        );
    }

    #[test]
    fn missing_or_ambiguous_quotes_are_not_placed() {
        assert_eq!(locate(SRC, "(sort-by :total)"), None);
        assert_eq!(locate(SRC, "map"), None);
        assert_eq!(locate(SRC, "(map"), None, "occurs twice");
        assert_eq!(locate(SRC, ""), None);
    }
}
