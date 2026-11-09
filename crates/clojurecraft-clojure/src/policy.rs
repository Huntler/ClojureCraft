//! What the evaluation harness permits in candidate code, read from the harness source itself so
//! that checks made before execution (and documentation filters) can never disagree with the
//! checks made inside the sandbox.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use crate::HARNESS_SOURCE;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessPolicy {
    /// Namespaces candidate code may require, including `clojure.core`.
    pub allowed_namespaces: BTreeSet<String>,
    /// `clojure.core` symbols candidate code may not use.
    pub denied_core: BTreeSet<String>,
    /// Java classes whose static members may be used (`Math/abs`).
    pub allowed_classes: BTreeSet<String>,
    /// Top-level forms allowed after the `ns` form.
    pub allowed_top_level: BTreeSet<String>,
}

/// The elements of the set literal bound by `(def <name> '#{...})` in the harness.
fn harness_set(name: &str) -> BTreeSet<String> {
    let src = HARNESS_SOURCE;
    let Some(start) = src
        .find(&format!("(def {name}\n"))
        .or_else(|| src.find(&format!("(def {name} ")))
    else {
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

/// Whether the namespace part of `ns/name` names a Java class (`Instant`, `java.time.Instant`)
/// rather than a Clojure namespace: by convention its last segment is capitalized.
pub fn is_class_name(s: &str) -> bool {
    s.rsplit('.')
        .next()
        .is_some_and(|last| last.starts_with(|c: char| c.is_ascii_uppercase()))
}

/// The harness's rules, parsed once.
pub fn harness_policy() -> &'static HarnessPolicy {
    static POLICY: OnceLock<HarnessPolicy> = OnceLock::new();
    POLICY.get_or_init(|| HarnessPolicy {
        allowed_namespaces: harness_set("allowed-namespaces"),
        denied_core: harness_set("denied-core"),
        allowed_classes: harness_set("allowed-classes"),
        allowed_top_level: harness_set("allowed-top-level"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_names_are_told_from_namespaces() {
        assert!(is_class_name("Instant") && is_class_name("java.time.Instant"));
        assert!(!is_class_name("str") && !is_class_name("clojure.string"));
    }

    #[test]
    fn every_list_is_read_from_the_harness() {
        let p = harness_policy();
        assert!(p.allowed_namespaces.contains("clojure.core"));
        assert!(p.allowed_namespaces.contains("clojure.string"));
        assert!(!p.allowed_namespaces.contains("clojure.java.io"));
        for denied in ["eval", "slurp", "future", "proxy", "with-redefs"] {
            assert!(p.denied_core.contains(denied), "{denied}");
        }
        assert!(!p.denied_core.contains("map"));
        for interop in [".", "..", "new", "memfn"] {
            assert!(!p.denied_core.contains(interop), "{interop}");
        }
        assert!(p.allowed_classes.contains("Math"));
        assert_eq!(
            p.allowed_top_level,
            ["declare", "def", "defn", "defn-"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect()
        );
    }
}
