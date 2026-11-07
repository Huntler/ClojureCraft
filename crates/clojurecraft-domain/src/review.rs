//! The implementation an engineer accepted for deeper testing, and the rules that keep it from
//! being replaced or forgotten without notice.
//!
//! Accepting records exactly the code that passed its checks together with the content it was
//! checked against. Later changes (manual edits, regenerated code, fixes) are compared with it, so
//! the accepted version is never replaced silently and stays recoverable from history.

use serde::{Deserialize, Serialize};

use crate::fingerprint::{Basis, code_fingerprint};
use crate::function::FunctionDocument;
use crate::ids::ValidationId;
use crate::results::ValidationReport;
use crate::time::Timestamp;

/// Code accepted for edge-case testing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedCode {
    /// The accepted source exactly.
    pub source: String,
    /// The specification, code, and accepted tests the passing report was computed from.
    pub basis: Basis,
    /// The passing validation report that allowed the acceptance.
    pub report: ValidationId,
    pub accepted_at: Timestamp,
}

/// Why the current implementation cannot be accepted for edge-case testing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AcceptBlocker {
    NoImplementation,
    /// No check has been run on exactly the current content.
    NotChecked,
    /// The newest check of the current content did not pass.
    ChecksFailed,
}

impl AcceptBlocker {
    pub fn describe(&self) -> &'static str {
        match self {
            Self::NoImplementation => "There is no implementation to test yet.",
            Self::NotChecked => {
                "Run the checks on the current code first; edge cases are tested once they pass."
            }
            Self::ChecksFailed => "The current code does not pass its checks yet.",
        }
    }
}

impl FunctionDocument {
    /// The newest report about exactly the current specification, code, and accepted tests, if
    /// it passed; otherwise why the code cannot be accepted. `reports` are newest first.
    pub fn passing_report<'a>(
        &self,
        reports: &'a [ValidationReport],
    ) -> Result<&'a ValidationReport, AcceptBlocker> {
        if self.implementation.is_none() {
            return Err(AcceptBlocker::NoImplementation);
        }
        let basis = self.basis();
        match reports.iter().find(|r| r.basis == basis) {
            None => Err(AcceptBlocker::NotChecked),
            Some(r) if r.passed() => Ok(r),
            Some(_) => Err(AcceptBlocker::ChecksFailed),
        }
    }

    /// Records the current implementation as accepted, using a passing report about exactly the
    /// current content.
    pub fn accept_implementation(&mut self, report: &ValidationReport) -> Result<(), AcceptBlocker> {
        let source = match &self.implementation {
            Some(i) => i.source.clone(),
            None => return Err(AcceptBlocker::NoImplementation),
        };
        let basis = self.basis();
        if report.basis != basis {
            return Err(AcceptBlocker::NotChecked);
        }
        if !report.passed() {
            return Err(AcceptBlocker::ChecksFailed);
        }
        self.accepted = Some(AcceptedCode {
            source,
            basis,
            report: report.id,
            accepted_at: Timestamp::now(),
        });
        Ok(())
    }

    /// Whether the implementation is exactly the accepted code.
    pub fn is_accepted_code(&self) -> bool {
        self.accepted
            .as_ref()
            .is_some_and(|a| code_fingerprint(Some(&a.source)) == self.code_fingerprint())
    }

    /// Whether code was accepted and the implementation has changed since.
    pub fn differs_from_accepted(&self) -> bool {
        self.accepted.is_some() && !self.is_accepted_code()
    }
}

#[cfg(test)]
mod tests {
    use crate::*;

    fn doc() -> FunctionDocument {
        let mut d = FunctionDocument::new("totals");
        d.description = "Sum the numbers".into();
        d.implementation = Some(Implementation::manual(
            "(ns totals.core)\n(defn totals [xs] (reduce + xs))",
        ));
        d
    }

    fn report(doc: &FunctionDocument, status: CheckStatus) -> ValidationReport {
        ValidationReport {
            id: ValidationId::generate(),
            created_at: Timestamp::now(),
            basis: doc.basis(),
            environment: ExecutionEnvironment {
                clojure_version: "1.12.6".into(),
                java_version: "25".into(),
                platform: "linux-x86_64".into(),
            },
            checks: vec![Check {
                subject: CheckSubject::Load,
                status,
                expected: None,
                actual: None,
                diagnostics: vec![],
                duration_ms: None,
            }],
            attempt: None,
            run: None,
            source: None,
            settings: None,
        }
    }

    #[test]
    fn only_a_passing_check_of_exactly_the_current_content_allows_acceptance() {
        let mut d = doc();
        assert_eq!(d.passing_report(&[]).err(), Some(AcceptBlocker::NotChecked));
        let failed = report(&d, CheckStatus::Failed);
        assert_eq!(
            d.passing_report(std::slice::from_ref(&failed)).err(),
            Some(AcceptBlocker::ChecksFailed)
        );
        assert_eq!(d.accept_implementation(&failed), Err(AcceptBlocker::ChecksFailed));

        let passed = report(&d, CheckStatus::Passed);
        // Newest first: the newest report about the current content is the one that counts.
        let reports = [passed.clone(), failed];
        assert_eq!(d.passing_report(&reports).map(|r| r.id), Ok(passed.id));

        // A report about other content (here: before an accepted test was added) does not count.
        let mut with_test = d.clone();
        with_test.tests.push(TestCase {
            id: TestCaseId::generate(),
            name: "empty".into(),
            input: Edn::new("[]"),
            expectation: Expectation::Value {
                expected: Edn::new("0"),
            },
            status: TestStatus::Accepted,
            origin: TestOrigin::User,
            notes: String::new(),
        });
        assert_eq!(
            with_test.passing_report(&reports).err(),
            Some(AcceptBlocker::NotChecked)
        );
        assert_eq!(
            with_test.accept_implementation(&passed),
            Err(AcceptBlocker::NotChecked)
        );
        assert!(with_test.accepted.is_none());

        d.accept_implementation(&passed).unwrap();
        let accepted = d.accepted.clone().unwrap();
        assert_eq!(accepted.basis, d.basis());
        assert_eq!(accepted.report, passed.id);
        assert!(d.is_accepted_code() && !d.differs_from_accepted());
    }

    #[test]
    fn a_manual_edit_leaves_the_accepted_version_and_makes_its_checks_stale() {
        let mut d = doc();
        let passed = report(&d, CheckStatus::Passed);
        d.accept_implementation(&passed).unwrap();
        let accepted_source = d.accepted.as_ref().unwrap().source.clone();

        if let Some(i) = &mut d.implementation {
            i.source.push_str("\n;; tweaked");
        }
        assert!(d.differs_from_accepted());
        // The accepted version is untouched and its checks no longer describe the code.
        assert_eq!(d.accepted.as_ref().unwrap().source, accepted_source);
        assert_eq!(
            passed.freshness(&d.basis()),
            Freshness::Stale(vec![Component::Code])
        );
        assert_eq!(
            d.passing_report(std::slice::from_ref(&passed)).err(),
            Some(AcceptBlocker::NotChecked)
        );
        // Acceptance itself is not a material change: it never makes results stale.
        let before = d.basis();
        d.accepted = None;
        assert_eq!(d.basis(), before);
    }
}
