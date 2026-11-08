//! Short Clojure idioms for tasks small models often get wrong, chosen by words in the
//! specification. Each one is a single copyable form, which a small model uses far more reliably
//! than a page of reference documentation.

use clojurecraft_domain::FunctionDocument;
use clojurecraft_inference::Section;

/// Prompt label of the idioms section.
pub const LABEL: &str = "Idioms";

/// (words that select the idiom, the idiom). An idiom is chosen when any word occurs in the
/// lower-cased description or steps.
const IDIOMS: &[(&[&str], &str)] = &[
    (
        &[
            "highest first",
            "descending",
            "largest first",
            "biggest first",
            "most first",
            "decreasing",
        ],
        "Sort descending by a number, ties by another key ascending: \
         (sort-by (fn [m] [(- (:score m)) (:name m)]) xs). Descending by any comparable: \
         (sort-by :k #(compare %2 %1) xs).",
    ),
    (
        &[
            "optional",
            "if present",
            "if it exists",
            "if the",
            "when present",
            "only if",
            "omit",
        ],
        "Add keys only when a value exists, in one expression (data is immutable, assoc returns a \
         new map): (cond-> {:a a} b (assoc :b b) c (assoc :c c)).",
    ),
    (
        &["parse", "number", "integer", "decimal", "numeric"],
        "Text to numbers: (parse-long \"12\") => 12 and (parse-double \"7.5\") => 7.5; both return \
         nil for text that is not a number. To keep integers as integers: \
         (if (clojure.string/includes? s \".\") (parse-double s) (parse-long s)).",
    ),
    (
        &["line"],
        "Split text into lines and drop blank ones: \
         (remove clojure.string/blank? (clojure.string/split-lines s)).",
    ),
    (
        &["token", "whitespace", "split", "word"],
        "Split on whitespace and destructure optional parts: \
         (let [[a b & more] (clojure.string/split (clojure.string/trim s) #\"\\s+\")] …); \
         missing parts are nil. Split \"3@10\" on a character: (clojure.string/split t #\"@\") => [\"3\" \"10\"].",
    ),
    (
        &["suffix", "unit", "trailing", "strip", "remove the"],
        "Remove a suffix: (clojure.string/replace s #\"kg$\" \"\"); test it with \
         (clojure.string/ends-with? s \"kg\").",
    ),
    (
        &["group", "per ", "each customer", "by key"],
        "Group and aggregate: (update-vals (group-by :k xs) (fn [ms] (reduce + 0 (map :v ms)))). \
         Iterate a grouped map with (for [[k ms] grouped] …); each entry is a [key values] pair.",
    ),
    (
        &["count", "how often", "occurrence", "frequency"],
        "Count occurrences: (frequencies xs) => {value count}.",
    ),
    (
        &["sum", "total", "add up"],
        "Sum a key: (reduce + 0 (map :amount xs)); an empty collection gives 0.",
    ),
    (
        &["vector"],
        "Return a vector, not a lazy sequence: (mapv f xs), (filterv pred xs), (vec (take n xs)), \
         (into [] (remove pred) xs).",
    ),
    (
        &["lower", "upper", "case", "normalize", "normalise"],
        "Change case: (clojure.string/lower-case s), (clojure.string/upper-case s).",
    ),
    (
        &["contain", "includes", "substring"],
        "Substring test: (clojure.string/includes? s \"x\"); clojure.string has no contains?.",
    ),
    (
        &["date", "week", "day of"],
        "Integer division is quot, never /: (quot 7 2) => 3; (/ 7 2) is the ratio 7/2. Remainder: (mod a b).",
    ),
];

/// The idioms the specification's wording calls for, as an optional prompt section.
pub fn section(doc: &FunctionDocument) -> Option<Section> {
    let mut text = doc.description.to_lowercase();
    for s in &doc.steps {
        text.push('\n');
        text.push_str(&s.text.to_lowercase());
    }
    let chosen: Vec<&str> = IDIOMS
        .iter()
        .filter(|(words, _)| words.iter().any(|w| text.contains(w)))
        .map(|(_, idiom)| *idiom)
        .collect();
    if chosen.is_empty() {
        return None;
    }
    Some(Section::optional(
        LABEL,
        // Kept longer than the documentation, which is longer and less direct.
        0,
        format!(
            "Clojure idioms that fit this task (use them where they apply):\n- {}",
            chosen.join("\n- ")
        ),
    ))
}

#[cfg(test)]
mod tests {
    use clojurecraft_domain::Step;

    use super::*;

    #[test]
    fn idioms_are_chosen_by_the_specification_wording() {
        let mut d = FunctionDocument::new("top");
        d.description = "Return the names of the three players with the highest score.".into();
        assert!(section(&d).is_none());
        d.steps = vec![Step::new("Sort players by :score, highest first; ties by :name.")];
        let s = section(&d).unwrap();
        assert!(
            s.text
                .contains("(sort-by (fn [m] [(- (:score m)) (:name m)]) xs)"),
            "{}",
            s.text
        );
        assert!(!s.text.contains("frequencies"), "{}", s.text);
    }
}
