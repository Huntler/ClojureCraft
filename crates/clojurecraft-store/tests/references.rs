//! Referenced example inputs across restarts and upstream changes.

// Test helpers outside #[test] functions fail loudly on setup errors by design.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;

use clojurecraft_domain::*;
use clojurecraft_store::*;

fn open(dir: &Path) -> Store {
    Store::open_dir(dir, Arc::new(SecretRegistry::new())).unwrap().0
}

fn checkpoint() -> RevisionReason {
    RevisionReason::new(RevisionKind::Checkpoint)
}

/// An upstream function with one committed revision, and a downstream function whose only
/// example takes its input from that revision's example, with a recorded snapshot.
fn linked(store: &mut Store) -> (FunctionDocument, FunctionDocument, RevisionId) {
    let mut up = store.create_function("paid orders").unwrap().document;
    up.examples = vec![Example::direct(
        "mixed",
        Edn::new("[{:id 1 :paid true} {:id 2}]"),
        Edn::new("[{:id 1 :paid true}]"),
    )];
    up.implementation = Some(Implementation::manual(
        "(ns p.core)\n(defn p [xs] (filterv :paid xs))",
    ));
    let r1 = store.commit_revision(&up, 1, checkpoint()).unwrap().revision().id;

    let mut down = store.create_function("count paid").unwrap().document;
    let example = down.add_example("from paid orders");
    let source = OutputReference {
        function: up.id,
        revision: RevisionPin::Latest,
        example: up.examples[0].id,
    };
    down.set_input_reference(example, source.clone());
    down.record_reference_snapshot(
        example,
        &source,
        ReferencedValue {
            value: Edn::new("[{:id 1, :paid true}]"),
            revision: r1,
            resolved_at: Timestamp(100),
        },
    );
    down.example_mut(example).unwrap().expected = Edn::new("1");
    store.save_draft(&down, 1).unwrap();
    (up, down, r1)
}

#[test]
fn references_and_their_snapshots_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (up, down, r1) = {
        let mut store = open(dir.path());
        linked(&mut store)
    };
    let store = open(dir.path());
    let reopened = store.load_draft(down.id).unwrap().document;
    assert_eq!(reopened, down);
    assert_eq!(reopened.spec_fingerprint(), down.spec_fingerprint());
    let input = &reopened.examples[0].input;
    assert_eq!(input.reference().map(|r| r.function), Some(up.id));
    assert_eq!(input.snapshot().map(|s| s.revision), Some(r1));
    assert_eq!(store.check_references(down.id).unwrap(), vec![]);
    assert_eq!(store.reference_edges().unwrap(), vec![(down.id, up.id)]);
    assert_eq!(store.dependents(up.id).unwrap()[0].function, down.id);
}

#[test]
fn upstream_changes_leave_downstream_examples_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(dir.path());
    let (mut up, down, r1) = linked(&mut store);
    let basis = down.basis();

    // The upstream function changes its code and its example, and records a new revision.
    up.implementation = Some(Implementation::manual("(ns p.core)\n(defn p [xs] (vec xs))"));
    up.examples[0].input = ExampleInput::Direct {
        value: Edn::new("[{:id 9}]"),
    };
    let r2 = store.commit_revision(&up, 2, checkpoint()).unwrap().revision().id;
    assert_ne!(r1, r2);
    drop(store);

    let store = open(dir.path());
    let stored = store.load_draft(down.id).unwrap().document;
    assert_eq!(stored.examples[0].input.snapshot().map(|s| s.revision), Some(r1));
    assert_eq!(
        stored.basis().freshness(&basis),
        Freshness::Current,
        "results computed with the recorded value stay current"
    );
    // The newer revision is visible, so the editor can offer to refresh.
    assert_eq!(store.revisions(up.id).unwrap()[0].id, r2);
}

#[test]
fn unsaved_references_are_checked_from_the_editors_copy() {
    let mut store = Store::open_in_memory(Arc::new(SecretRegistry::new())).unwrap();
    let (up, mut down, _) = linked(&mut store);
    // Not yet saved: the example now points at an upstream example that does not exist.
    let missing = ExampleId::generate();
    let id = down.examples[0].id;
    down.set_input_reference(
        id,
        OutputReference {
            function: up.id,
            revision: RevisionPin::Latest,
            example: missing,
        },
    );
    assert_eq!(store.check_references(down.id).unwrap(), vec![]);
    assert_eq!(
        store.check_document_references(&down).unwrap(),
        vec![ReferenceProblem::MissingExample {
            example: id,
            upstream_example: missing,
        }]
    );
}
