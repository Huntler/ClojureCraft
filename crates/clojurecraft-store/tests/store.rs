//! Storage contracts: persistence across restarts, ordered draft writes, append-only history,
//! reference integrity, result staleness, and credential exclusion.

// Test helpers outside #[test] functions fail loudly on setup errors by design.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;

use clojurecraft_domain::format::FormatError;
use clojurecraft_domain::*;
use clojurecraft_store::*;

fn open(dir: &Path) -> Store {
    Store::open_dir(dir, Arc::new(SecretRegistry::new())).unwrap().0
}

fn spec(doc: &mut FunctionDocument) {
    doc.description = "Sum amounts per customer.".into();
    doc.steps = vec![Step::new("group by :customer"), Step::new("sum :amount")];
    doc.examples = vec![Example::direct(
        "basic",
        Edn::new("[{:customer 1 :amount 2}]"),
        Edn::new("{1 2}"),
    )];
}

fn report(basis: Basis, diagnostics: Vec<String>) -> ValidationReport {
    ValidationReport {
        id: ValidationId::generate(),
        created_at: Timestamp::now(),
        basis,
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
            diagnostics,
            duration_ms: None,
        }],
        attempt: None,
        run: None,
        source: None,
        settings: None,
    }
}

#[test]
fn drafts_and_history_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let mut store = open(dir.path());
        let mut doc = store.create_function("totals").unwrap().document;
        spec(&mut doc);
        store
            .commit_revision(&doc, 1, RevisionReason::new(RevisionKind::Checkpoint))
            .unwrap();
        doc.description.push_str(" Unsaved idea.");
        assert_eq!(store.save_draft(&doc, 2).unwrap(), SaveOutcome::Saved);
        doc.id
    };
    let store = open(dir.path());
    let draft = store.load_draft(id).unwrap();
    assert!(draft.document.description.ends_with("Unsaved idea."));
    assert_eq!(draft.seq, 2);
    assert!(draft.has_unrevisioned_changes);
    assert_eq!(store.revisions(id).unwrap().len(), 1);
    let listed = store.list_functions(Listing::Active, "").unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].revision_count, 1);
}

#[test]
fn out_of_date_draft_writes_are_ignored() {
    let mut store = Store::open_in_memory(Arc::new(SecretRegistry::new())).unwrap();
    let mut doc = store.create_function("f").unwrap().document;
    doc.description = "newer".into();
    store.save_draft(&doc, 5).unwrap();
    let mut older = doc.clone();
    older.description = "older".into();
    assert_eq!(
        store.save_draft(&older, 3).unwrap(),
        SaveOutcome::Superseded { stored_seq: 5 }
    );
    assert_eq!(store.load_draft(doc.id).unwrap().document.description, "newer");
    assert!(matches!(
        store.commit_revision(&older, 4, RevisionReason::new(RevisionKind::Checkpoint)),
        Err(StoreError::StaleSequence { stored: 5, given: 4 })
    ));
    assert!(store.revisions(doc.id).unwrap().is_empty());
}

#[test]
fn committing_unchanged_content_adds_no_revision() {
    let mut store = Store::open_in_memory(Arc::new(SecretRegistry::new())).unwrap();
    let mut doc = store.create_function("f").unwrap().document;
    spec(&mut doc);
    let reason = || RevisionReason::new(RevisionKind::Checkpoint);
    let first = store.commit_revision(&doc, 1, reason()).unwrap();
    assert!(matches!(first, CommitOutcome::Created(ref r) if r.number == 1 && r.parent.is_none()));
    let again = store.commit_revision(&doc, 2, reason()).unwrap();
    assert_eq!(again, CommitOutcome::Unchanged(first.revision().clone()));
    doc.name = "renamed".into();
    let second = store.commit_revision(&doc, 3, reason()).unwrap();
    assert_eq!(second.revision().number, 2);
    assert_eq!(second.revision().parent, Some(first.revision().id));
    let draft = store.load_draft(doc.id).unwrap();
    assert!(!draft.has_unrevisioned_changes);
    assert_eq!(draft.base_revision, Some(second.revision().id));
}

#[test]
fn restoring_a_revision_keeps_all_later_history() {
    let mut store = Store::open_in_memory(Arc::new(SecretRegistry::new())).unwrap();
    let mut doc = store.create_function("f").unwrap().document;
    spec(&mut doc);
    let r1 = store
        .commit_revision(&doc, 1, RevisionReason::new(RevisionKind::Checkpoint))
        .unwrap()
        .revision()
        .clone();
    let r1_doc = doc.clone();
    doc.implementation = Some(Implementation::manual("(ns f)\n(defn f [x] x)"));
    let r2 = store
        .commit_revision(&doc, 2, RevisionReason::new(RevisionKind::CodeEdited))
        .unwrap()
        .revision()
        .clone();
    doc.description = "work in progress".into();
    store.save_draft(&doc, 3).unwrap();
    let pre_restore = doc.clone();

    assert!(matches!(
        store.restore_revision(doc.id, r1.id, 2),
        Err(StoreError::StaleSequence { .. })
    ));
    let restored = store.restore_revision(doc.id, r1.id, 4).unwrap();
    assert_eq!(restored.document, r1_doc);
    assert_eq!(restored.seq, 4);
    assert!(!restored.has_unrevisioned_changes);

    let history = store.revisions(doc.id).unwrap();
    let numbers: Vec<_> = history.iter().map(|r| r.number).collect();
    assert_eq!(numbers, [4, 3, 2, 1]);
    assert_eq!(history[1].reason.kind, RevisionKind::BeforeRestore);
    assert_eq!(
        history[0].reason.kind,
        RevisionKind::Restored {
            from: r1.id,
            from_number: 1
        }
    );
    assert_eq!(store.load_revision(history[1].id).unwrap().document, pre_restore);
    assert_eq!(store.load_revision(r2.id).unwrap().summary, r2);
    assert_eq!(restored.base_revision, Some(history[0].id));

    // A delayed autosave of the pre-restore draft cannot overwrite the restored draft.
    assert_eq!(
        store.save_draft(&pre_restore, 3).unwrap(),
        SaveOutcome::Superseded { stored_seq: 4 }
    );
    assert_eq!(store.load_draft(doc.id).unwrap().document, r1_doc);
}

#[test]
fn history_and_results_are_append_only() {
    let mut store = Store::open_in_memory(Arc::new(SecretRegistry::new())).unwrap();
    let doc = store.create_function("f").unwrap().document;
    let r = store
        .commit_revision(&doc, 1, RevisionReason::new(RevisionKind::Checkpoint))
        .unwrap();
    store
        .record_validation(doc.id, &report(doc.basis(), vec![]))
        .unwrap();
    let conn = store.connection();
    for sql in [
        "UPDATE revisions SET document = '{}'",
        "UPDATE validation_reports SET report = '{}'",
    ] {
        let err = conn.execute(sql, []).unwrap_err();
        assert!(err.to_string().contains("immutable"), "{sql}: {err}");
    }
    assert_eq!(store.load_revision(r.revision().id).unwrap().document, doc);
}

#[test]
fn changing_content_marks_retained_results_stale() {
    let mut store = Store::open_in_memory(Arc::new(SecretRegistry::new())).unwrap();
    let mut doc = store.create_function("f").unwrap().document;
    spec(&mut doc);
    doc.implementation = Some(Implementation::manual("(ns f)\n(defn f [x] x)"));
    store.save_draft(&doc, 1).unwrap();
    let passing = store
        .record_validation(doc.id, &report(doc.basis(), vec![]))
        .unwrap();
    let original_basis = doc.basis();

    doc.examples[0].expected = Edn::new("{1 3}");
    store.save_draft(&doc, 2).unwrap();
    let current = store.load_draft(doc.id).unwrap().document.basis();
    let reports = store.validation_reports(doc.id).unwrap();
    assert_eq!(reports, vec![passing.clone()], "stale results are retained");
    assert_eq!(
        reports[0].freshness(&current),
        Freshness::Stale(vec![Component::Specification])
    );
    assert!(store.validation_for(doc.id, &current).unwrap().is_empty());
    assert_eq!(
        store.validation_for(doc.id, &original_basis).unwrap(),
        vec![passing]
    );
}

/// Builds an upstream function with one committed example and a downstream function whose
/// example input references it.
fn upstream_and_downstream(store: &mut Store) -> (FunctionDocument, FunctionDocument, RevisionId) {
    let mut up = store.create_function("upstream").unwrap().document;
    spec(&mut up);
    let rev = store
        .commit_revision(&up, 1, RevisionReason::new(RevisionKind::Checkpoint))
        .unwrap()
        .revision()
        .id;
    let mut down = store.create_function("downstream").unwrap().document;
    down.examples = vec![Example {
        id: ExampleId::generate(),
        name: "from upstream".into(),
        input: ExampleInput::FunctionOutput {
            source: OutputReference {
                function: up.id,
                revision: RevisionPin::Latest,
                example: up.examples[0].id,
            },
            snapshot: None,
        },
        expected: Edn::new("[]"),
    }];
    store.save_draft(&down, 1).unwrap();
    (up, down, rev)
}

#[test]
fn references_are_checked_and_protect_their_targets() {
    let mut store = Store::open_in_memory(Arc::new(SecretRegistry::new())).unwrap();
    let (up, mut down, up_rev) = upstream_and_downstream(&mut store);
    let example = down.examples[0].id;
    assert_eq!(store.check_references(down.id).unwrap(), vec![]);
    assert_eq!(
        store.dependents(up.id).unwrap(),
        vec![Dependent {
            function: down.id,
            name: "downstream".into()
        }]
    );

    store.trash_function(up.id).unwrap();
    assert_eq!(
        store.check_references(down.id).unwrap(),
        vec![ReferenceProblem::FunctionInTrash {
            example,
            function: up.id
        }]
    );
    assert!(matches!(
        store.purge_function(up.id),
        Err(StoreError::Referenced { ref by, .. }) if by == &["downstream".to_owned()]
    ));
    store.restore_function(up.id).unwrap();

    // Pinned to a revision of a different function, and to an example that does not exist.
    let other_rev = store
        .commit_revision(&down, 2, RevisionReason::new(RevisionKind::Checkpoint))
        .unwrap()
        .revision()
        .id;
    let ExampleInput::FunctionOutput { source, .. } = &mut down.examples[0].input else {
        unreachable!()
    };
    source.revision = RevisionPin::Pinned(other_rev);
    store.save_draft(&down, 3).unwrap();
    assert_eq!(
        store.check_references(down.id).unwrap(),
        vec![ReferenceProblem::RevisionOfOtherFunction {
            example,
            revision: other_rev
        }]
    );
    let ExampleInput::FunctionOutput { source, .. } = &mut down.examples[0].input else {
        unreachable!()
    };
    source.revision = RevisionPin::Pinned(up_rev);
    source.example = ExampleId::generate();
    let missing = source.example;
    store.save_draft(&down, 4).unwrap();
    assert_eq!(
        store.check_references(down.id).unwrap(),
        vec![ReferenceProblem::MissingExample {
            example,
            upstream_example: missing
        }]
    );

    // Removing the reference from the draft is not enough: revision 1 of the downstream
    // function still references the upstream function, so history stays resolvable.
    down.examples.clear();
    store.save_draft(&down, 5).unwrap();
    store.trash_function(up.id).unwrap();
    assert!(matches!(
        store.purge_function(up.id),
        Err(StoreError::Referenced { .. })
    ));

    // Once the dependent is gone too, the upstream function can be purged.
    store.trash_function(down.id).unwrap();
    store.purge_function(down.id).unwrap();
    store.purge_function(up.id).unwrap();
    assert!(store.list_functions(Listing::Trash, "").unwrap().is_empty());
    let orphans: i64 = store
        .connection()
        .query_row(
            "SELECT (SELECT count(*) FROM revisions) + (SELECT count(*) FROM output_references)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphans, 0);
}

#[test]
fn missing_upstream_functions_are_reported() {
    let mut store = Store::open_in_memory(Arc::new(SecretRegistry::new())).unwrap();
    let mut doc = store.create_function("f").unwrap().document;
    let ghost = FunctionId::generate();
    doc.examples = vec![Example {
        id: ExampleId::generate(),
        name: "x".into(),
        input: ExampleInput::FunctionOutput {
            source: OutputReference {
                function: ghost,
                revision: RevisionPin::Latest,
                example: ExampleId::generate(),
            },
            snapshot: None,
        },
        expected: Edn::new("nil"),
    }];
    store.save_draft(&doc, 1).unwrap();
    assert_eq!(
        store.check_references(doc.id).unwrap(),
        vec![ReferenceProblem::MissingFunction {
            example: doc.examples[0].id,
            function: ghost
        }]
    );
    let fresh = store.create_function("no history").unwrap().document;
    doc.examples[0].input = ExampleInput::FunctionOutput {
        source: OutputReference {
            function: fresh.id,
            revision: RevisionPin::Latest,
            example: ExampleId::generate(),
        },
        snapshot: None,
    };
    store.save_draft(&doc, 2).unwrap();
    assert_eq!(
        store.check_references(doc.id).unwrap(),
        vec![ReferenceProblem::NoRevisions {
            example: doc.examples[0].id,
            function: fresh.id
        }]
    );
}

#[test]
fn credentials_never_enter_history_or_results() {
    let secrets = Arc::new(SecretRegistry::new());
    let key = "session-key-9f8e7d6c5b4a3210";
    secrets.register(key);
    let mut store = Store::open_in_memory(secrets).unwrap();
    let mut doc = store.create_function("f").unwrap().document;
    spec(&mut doc);
    store.save_draft(&doc, 1).unwrap();

    let mut leaking = doc.clone();
    leaking.description = format!("pasted from a log: Authorization: Bearer {key}");
    assert!(matches!(
        store.save_draft(&leaking, 2),
        Err(StoreError::SecretDetected { .. })
    ));
    assert!(matches!(
        store.commit_revision(&leaking, 2, RevisionReason::new(RevisionKind::Checkpoint)),
        Err(StoreError::SecretDetected { .. })
    ));
    assert_eq!(store.load_draft(doc.id).unwrap().document, doc);
    assert!(store.revisions(doc.id).unwrap().is_empty());

    let token = format!("hf_{}", "Zx9".repeat(12));
    let stored = store
        .record_validation(
            doc.id,
            &report(doc.basis(), vec![format!("401 using {key} and {token}")]),
        )
        .unwrap();
    assert_eq!(
        stored.checks[0].diagnostics,
        ["401 using [redacted] and [redacted]"]
    );
    let raw: String = store
        .connection()
        .query_row("SELECT group_concat(report) FROM validation_reports", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(!raw.contains(key) && !raw.contains(&token));
}

#[test]
fn unreadable_drafts_are_reported_and_recoverable_from_history() {
    let mut store = Store::open_in_memory(Arc::new(SecretRegistry::new())).unwrap();
    let mut doc = store.create_function("f").unwrap().document;
    spec(&mut doc);
    store
        .commit_revision(&doc, 1, RevisionReason::new(RevisionKind::Checkpoint))
        .unwrap();
    assert!(matches!(
        store.recover_draft(doc.id),
        Err(StoreError::DraftReadable)
    ));

    // Written by a newer application version: reported, and never overwritten by recovery.
    store
        .connection()
        .execute("UPDATE functions SET draft_format = 99", [])
        .unwrap();
    assert!(matches!(
        store.load_draft(doc.id),
        Err(StoreError::Format {
            source: FormatError::TooNew { found: 99, .. },
            ..
        })
    ));
    assert!(store.recover_draft(doc.id).is_err());

    store
        .connection()
        .execute(
            "UPDATE functions SET draft_format = 1, draft = '{\"id\": tru'",
            [],
        )
        .unwrap();
    assert!(matches!(
        store.load_draft(doc.id),
        Err(StoreError::Format {
            source: FormatError::Malformed { .. },
            ..
        })
    ));
    let recovered = store.recover_draft(doc.id).unwrap();
    assert_eq!(recovered.document, doc);
    assert_eq!(recovered.seq, 2);
}

#[test]
fn library_search_trash_and_duplication() {
    let mut store = Store::open_in_memory(Arc::new(SecretRegistry::new())).unwrap();
    let mut a = store.create_function("parse 100% invoices").unwrap().document;
    a.description = "Uses discount_rate".into();
    store.save_draft(&a, 1).unwrap();
    let b = store.create_function("totals").unwrap().document;

    let names = |s: &Store, l, q| -> Vec<String> {
        s.list_functions(l, q)
            .unwrap()
            .into_iter()
            .map(|f| f.name)
            .collect()
    };
    assert_eq!(names(&store, Listing::Active, "100%"), ["parse 100% invoices"]);
    assert_eq!(names(&store, Listing::Active, "t_rate"), ["parse 100% invoices"]);
    assert!(
        names(&store, Listing::Active, "0_%").is_empty(),
        "wildcards are literal"
    );
    assert_eq!(names(&store, Listing::Active, "TOTALS"), ["totals"]);

    let copy = store.duplicate_function(a.id, "parse invoices (copy)").unwrap();
    assert_ne!(copy.document.id, a.id);
    assert_eq!(copy.document.description, a.description);
    assert!(store.revisions(copy.document.id).unwrap().is_empty());

    store.trash_function(b.id).unwrap();
    assert_eq!(names(&store, Listing::Trash, ""), ["totals"]);
    assert!(matches!(
        store.purge_function(a.id),
        Err(StoreError::NotInTrash(_))
    ));
    store.restore_function(b.id).unwrap();
    assert_eq!(store.list_functions(Listing::Active, "").unwrap().len(), 3);
}
