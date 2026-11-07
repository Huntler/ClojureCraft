//! The accepted implementation in history: it is recorded as its own revision, survives later
//! edits and fixes, and can be recovered without losing anything recorded after it.

// Test helpers outside #[test] functions fail loudly on setup errors by design.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use clojurecraft_domain::*;
use clojurecraft_store::*;

const ACCEPTED: &str = "(ns totals.core)\n(defn totals [xs] (reduce + xs))\n";

fn passing(doc: &FunctionDocument) -> ValidationReport {
    ValidationReport {
        id: ValidationId::generate(),
        created_at: Timestamp::now(),
        basis: doc.basis(),
        environment: ExecutionEnvironment {
            clojure_version: "1.12.6".into(),
            java_version: "25.0.5".into(),
            platform: "linux-x86_64".into(),
        },
        checks: vec![Check {
            subject: CheckSubject::Load,
            status: CheckStatus::Passed,
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
fn the_previously_accepted_function_is_recoverable_from_history() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(SecretRegistry::new());
    let (mut store, _) = Store::open_dir(dir.path(), secrets.clone()).unwrap();
    let mut doc = store.create_function("totals").unwrap().document;
    doc.description = "Sum the numbers".into();
    doc.implementation = Some(Implementation::manual(ACCEPTED));
    let report = store.record_validation(doc.id, &passing(&doc)).unwrap();
    doc.accept_implementation(&report).unwrap();
    let accepted = store
        .commit_revision(
            &doc,
            1,
            RevisionReason::with_note(RevisionKind::Accepted, "Accepted for edge-case testing"),
        )
        .unwrap()
        .revision()
        .clone();
    let accepted_doc = doc.clone();

    // Later work: an accepted edge case and a manual edit.
    doc.tests.push(TestCase {
        id: TestCaseId::generate(),
        name: "empty".into(),
        input: Edn::new("[]"),
        expectation: Expectation::Value {
            expected: Edn::new("0"),
        },
        status: TestStatus::Accepted,
        origin: TestOrigin::Model,
        notes: String::new(),
    });
    store
        .commit_revision(&doc, 2, RevisionReason::new(RevisionKind::TestsAccepted))
        .unwrap();
    doc.implementation = Some(Implementation::manual("(ns totals.core)\n(defn totals [xs] 0)\n"));
    store.save_draft(&doc, 3).unwrap();
    assert!(doc.differs_from_accepted());
    drop(store);

    // After a restart the accepted revision is found by its kind and still holds the code.
    let (mut store, _) = Store::open_dir(dir.path(), secrets).unwrap();
    let history = store.revisions(doc.id).unwrap();
    let found = history
        .iter()
        .find(|r| r.reason.kind == RevisionKind::Accepted)
        .unwrap();
    assert_eq!(found.id, accepted.id);
    assert_eq!(found.reason.note, "Accepted for edge-case testing");
    let revision = store.load_revision(found.id).unwrap();
    assert_eq!(revision.document, accepted_doc);
    assert_eq!(
        revision.document.accepted.as_ref().map(|a| a.source.as_str()),
        Some(ACCEPTED)
    );
    // The passing report still describes exactly the accepted content.
    let reports = store.validation_for(doc.id, &found.basis).unwrap();
    assert_eq!(reports.first().map(|r| r.id), Some(report.id));

    let restored = store.restore_revision(doc.id, found.id, 4).unwrap();
    assert_eq!(restored.document, accepted_doc);
    assert!(restored.document.is_accepted_code());
    let after = store.revisions(doc.id).unwrap();
    // Nothing recorded after the acceptance was lost: the edited draft and the accepted case are
    // still in the history.
    assert_eq!(after.len(), history.len() + 2);
    assert_eq!(after[1].reason.kind, RevisionKind::BeforeRestore);
    let before_restore = store.load_revision(after[1].id).unwrap().document;
    assert_eq!(before_restore, doc);
}
