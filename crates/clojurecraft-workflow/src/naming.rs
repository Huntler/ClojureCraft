//! Clojure names for a function: its public symbol and namespace, derived from the display name.

/// Special forms and core macros that can never name a function.
const SPECIAL_FORMS: &[&str] = &[
    "def",
    "if",
    "do",
    "let",
    "let*",
    "quote",
    "var",
    "fn",
    "fn*",
    "loop",
    "loop*",
    "recur",
    "throw",
    "try",
    "catch",
    "finally",
    "monitor-enter",
    "monitor-exit",
    "new",
    "set!",
    "letfn*",
    "case*",
    "import*",
    "reify*",
    "deftype*",
];

/// Suffix that keeps a derived name from shadowing a `clojure.core` var or special form.
const COLLISION_SUFFIX: &str = "-fn";

/// The public function's symbol: the display name in lowercase kebab case, using only ASCII
/// letters, digits, and hyphens. `is_core` reports whether a name is a `clojure.core` var; such
/// names (and special forms) get a suffix, because shadowing them would break calls to the core
/// function inside the generated namespace.
pub fn function_symbol(name: &str, is_core: impl Fn(&str) -> bool) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            if gap && !out.is_empty() {
                out.push('-');
            }
            gap = false;
            out.push(c.to_ascii_lowercase());
        } else {
            gap = true;
        }
    }
    if out.is_empty() {
        out = "transform".into();
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert_str(0, "fn-");
    }
    if SPECIAL_FORMS.contains(&out.as_str()) || is_core(&out) {
        out.push_str(COLLISION_SUFFIX);
    }
    out
}

/// The namespace that holds the function.
pub fn namespace_for(symbol: &str) -> String {
    format!("{symbol}.core")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> bool {
        false
    }

    #[test]
    fn display_names_become_kebab_case_symbols() {
        assert_eq!(function_symbol("Order totals", none), "order-totals");
        assert_eq!(
            function_symbol("  Group BY customer_id!! ", none),
            "group-by-customer-id"
        );
        assert_eq!(function_symbol("2024 report", none), "fn-2024-report");
        assert_eq!(function_symbol("Größe", none), "gr-e");
        assert_eq!(function_symbol("", none), "transform");
        assert_eq!(function_symbol("—", none), "transform");
        assert_eq!(namespace_for("order-totals"), "order-totals.core");
    }

    #[test]
    fn core_names_and_special_forms_are_not_shadowed() {
        assert_eq!(
            function_symbol("Frequencies", |s| s == "frequencies"),
            "frequencies-fn"
        );
        assert_eq!(function_symbol("Loop", none), "loop-fn");
        assert_eq!(
            function_symbol("Frequencies per day", |s| s == "frequencies"),
            "frequencies-per-day"
        );
    }
}
