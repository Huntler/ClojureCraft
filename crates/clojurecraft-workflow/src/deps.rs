//! Which namespaces an implementation may require.
//!
//! *Supported* namespaces are the standard-library namespaces the evaluation harness accepts at
//! all. *Allowed* namespaces are the ones this function may use: the accepted contract's list, or
//! a small default set when there is no accepted contract. Code that needs a supported namespace
//! that is not allowed is held back until the user approves it, which adds the namespace to the
//! contract; unsupported namespaces are never accepted.

use std::collections::BTreeSet;

use clojurecraft_domain::{Contract, ContractStatus, FunctionDocument};

pub const CORE: &str = "clojure.core";

/// Allowed when the function has no accepted contract.
pub const DEFAULT_ALLOWED: [&str; 2] = ["clojure.string", "clojure.set"];

/// Conventional aliases offered to the model.
pub fn alias(namespace: &str) -> Option<&'static str> {
    Some(match namespace {
        "clojure.string" => "str",
        "clojure.set" => "set",
        "clojure.walk" => "walk",
        "clojure.edn" => "edn",
        "clojure.zip" => "zip",
        "clojure.data" => "data",
        _ => return None,
    })
}

/// Supported namespaces besides `clojure.core`, in order.
pub fn supported() -> BTreeSet<String> {
    clojurecraft_clojure::harness_policy()
        .allowed_namespaces
        .iter()
        .filter(|n| *n != CORE)
        .cloned()
        .collect()
}

pub fn is_supported(namespace: &str) -> bool {
    namespace == CORE || supported().contains(namespace)
}

/// Namespaces this function may require besides `clojure.core`. A project function may also use
/// every namespace its file requires, and the project's other namespaces that do not depend on
/// its own.
pub fn allowed(doc: &FunctionDocument) -> BTreeSet<String> {
    let mut allowed = contract_allowed(doc);
    if let Some(p) = &doc.project {
        allowed.extend(
            crate::project::file_requires(&p.file)
                .into_iter()
                .filter(|n| n != CORE),
        );
        allowed.extend(p.usable.iter().cloned());
    }
    allowed
}

fn contract_allowed(doc: &FunctionDocument) -> BTreeSet<String> {
    let supported = supported();
    match &doc.contract {
        Some(c) if c.status == ContractStatus::Accepted => c
            .allowed_dependencies
            .iter()
            .filter(|n| supported.contains(*n))
            .cloned()
            .collect(),
        _ => DEFAULT_ALLOWED.iter().map(|s| (*s).to_owned()).collect(),
    }
}

/// A new contract allowing what is allowed without one.
pub fn new_contract(status: ContractStatus) -> Contract {
    let mut c = Contract::empty(status);
    c.allowed_dependencies = DEFAULT_ALLOWED.iter().map(|s| (*s).to_owned()).collect();
    c
}

#[derive(Debug, PartialEq, Eq)]
pub enum ApprovalError {
    /// Only supported namespaces can be approved.
    Unsupported(String),
    /// A proposed contract is pending; it must be accepted or discarded first so the approval is
    /// not hidden in an unreviewed proposal.
    ProposalPending,
}

impl std::fmt::Display for ApprovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(n) => write!(f, "{n} is not a supported namespace"),
            Self::ProposalPending => f.write_str("accept or discard the proposed contract first"),
        }
    }
}

/// Allows `namespaces` for this function by adding them to the accepted contract, creating one
/// (with the default namespaces) when there is none. Only an explicit user action calls this.
pub fn approve(doc: &mut FunctionDocument, namespaces: &[String]) -> Result<(), ApprovalError> {
    let supported = supported();
    if let Some(n) = namespaces.iter().find(|n| !supported.contains(*n)) {
        return Err(ApprovalError::Unsupported(n.clone()));
    }
    let contract = match &mut doc.contract {
        Some(c) if c.status == ContractStatus::Proposed => return Err(ApprovalError::ProposalPending),
        Some(c) => c,
        None => doc.contract.insert(new_contract(ContractStatus::Accepted)),
    };
    for n in namespaces {
        if !contract.allowed_dependencies.contains(n) {
            contract.allowed_dependencies.push(n.clone());
        }
    }
    contract.allowed_dependencies.sort();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply_without_an_accepted_contract() {
        let mut doc = FunctionDocument::new("f");
        assert_eq!(
            allowed(&doc),
            BTreeSet::from(["clojure.set".into(), "clojure.string".into()])
        );
        doc.contract = Some(Contract::empty(ContractStatus::Proposed));
        assert_eq!(
            allowed(&doc).len(),
            2,
            "a proposal grants nothing and restricts nothing"
        );
        doc.contract = Some(Contract::empty(ContractStatus::Accepted));
        assert!(allowed(&doc).is_empty(), "an accepted contract's list is exact");
    }

    #[test]
    fn unsupported_entries_in_a_contract_are_ignored() {
        let mut doc = FunctionDocument::new("f");
        let mut c = Contract::empty(ContractStatus::Accepted);
        c.allowed_dependencies = vec!["clojure.java.io".into(), "clojure.walk".into()];
        doc.contract = Some(c);
        assert_eq!(allowed(&doc), BTreeSet::from(["clojure.walk".into()]));
    }

    #[test]
    fn approval_extends_or_creates_the_accepted_contract() {
        let mut doc = FunctionDocument::new("f");
        approve(&mut doc, &["clojure.walk".into()]).unwrap();
        let c = doc.contract.as_ref().unwrap();
        assert_eq!(c.status, ContractStatus::Accepted);
        assert_eq!(
            c.allowed_dependencies,
            ["clojure.set", "clojure.string", "clojure.walk"]
        );
        approve(&mut doc, &["clojure.walk".into(), "clojure.zip".into()]).unwrap();
        assert_eq!(doc.contract.as_ref().unwrap().allowed_dependencies.len(), 4);

        assert_eq!(
            approve(&mut doc, &["clojure.java.io".into()]),
            Err(ApprovalError::Unsupported("clojure.java.io".into()))
        );
        doc.contract = Some(Contract::empty(ContractStatus::Proposed));
        assert_eq!(
            approve(&mut doc, &["clojure.walk".into()]),
            Err(ApprovalError::ProposalPending)
        );
    }
}
