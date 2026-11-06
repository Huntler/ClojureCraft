//! Durability: autosave ordering and recovery from a writer killed mid-write.

// Test helpers outside #[test] functions fail loudly on setup errors by design.
#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clojurecraft_domain::*;
use clojurecraft_store::*;

const CHILD_ENV: &str = "CLOJURECRAFT_CRASH_TEST_DIR";
const STEPS: usize = 40;
/// Printed by the writer once its first revision is committed (libtest may prefix the line).
const WRITING_MARKER: &str = "crash-test-writer-running";

fn open(dir: &Path) -> Store {
    Store::open_dir(dir, Arc::new(SecretRegistry::new())).unwrap().0
}

#[test]
fn autosave_debounces_and_writes_the_newest_edit() {
    let dir = tempfile::tempdir().unwrap();
    let mut ui_store = open(dir.path());
    let doc = ui_store.create_function("f").unwrap().document;
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let saver = Autosaver::spawn(open(dir.path()), Duration::from_millis(50), move |e| {
        sink.lock().unwrap().push(e);
    });
    for seq in 1..=20 {
        let mut edited = doc.clone();
        edited.description = format!("edit {seq}");
        saver.submit(edited, seq);
    }
    // An older edit arriving late is ignored.
    let mut late = doc.clone();
    late.description = "late".into();
    saver.submit(late, 3);
    assert!(saver.flush());

    let draft = ui_store.load_draft(doc.id).unwrap();
    assert_eq!((draft.document.description.as_str(), draft.seq), ("edit 20", 20));
    assert_eq!(
        *events.lock().unwrap(),
        [AutosaveEvent::Saved {
            function: doc.id,
            seq: 20
        }],
        "rapid edits are coalesced into one write"
    );

    // A restore performed elsewhere wins over an autosave of an older edit.
    let mut newer = draft.document.clone();
    newer.description = "restored".into();
    ui_store.save_draft(&newer, 30).unwrap();
    let mut stale = draft.document.clone();
    stale.description = "typed before restore".into();
    saver.submit(stale, 21);
    assert!(saver.flush());
    assert_eq!(
        ui_store.load_draft(doc.id).unwrap().document.description,
        "restored"
    );
    assert!(matches!(
        events.lock().unwrap().last(),
        Some(AutosaveEvent::Superseded {
            seq: 21,
            stored_seq: 30,
            ..
        })
    ));

    // Dropping the saver writes what is still queued.
    let mut last = newer.clone();
    last.description = "written on shutdown".into();
    let saver = saver;
    saver.submit(last, 31);
    drop(saver);
    assert_eq!(
        ui_store.load_draft(doc.id).unwrap().document.description,
        "written on shutdown"
    );
}

#[test]
fn revisions_recorded_through_the_autosaver_are_ordered_with_draft_writes() {
    let dir = tempfile::tempdir().unwrap();
    let ui_store = open(dir.path());
    let mut store = open(dir.path());
    let doc = store.create_function("f").unwrap().document;
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    // A long quiet period keeps the older edit queued when the commit arrives.
    let saver = Autosaver::spawn(open(dir.path()), Duration::from_secs(30), move |e| {
        sink.lock().unwrap().push(e);
    });
    let mut typed = doc.clone();
    typed.description = "typed".into();
    saver.submit(typed.clone(), 1);
    let mut generated = typed.clone();
    generated.implementation = Some(Implementation::manual("(ns a)\n(defn f [x] x)"));
    saver.commit(generated.clone(), 2, RevisionReason::new(RevisionKind::Generated));
    assert!(saver.flush());

    let draft = ui_store.load_draft(doc.id).unwrap();
    assert_eq!((draft.document, draft.seq), (generated.clone(), 2));
    assert!(!draft.has_unrevisioned_changes);
    let history = ui_store.revisions(doc.id).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].reason.kind, RevisionKind::Generated);
    assert_eq!(
        *events.lock().unwrap(),
        [AutosaveEvent::Committed {
            function: doc.id,
            seq: 2,
            number: 1,
            created: true,
        }],
        "the replaced older edit is never written"
    );

    // A failed commit still saves the draft as an ordinary edit.
    events.lock().unwrap().clear();
    store.save_draft(&generated, 10).unwrap();
    let mut late = generated.clone();
    late.description = "late".into();
    saver.commit(late.clone(), 5, RevisionReason::new(RevisionKind::Generated));
    assert!(saver.flush());
    let events = events.lock().unwrap();
    assert!(
        matches!(
            &events[..],
            [
                AutosaveEvent::CommitFailed { seq: 5, .. },
                AutosaveEvent::Superseded {
                    seq: 5,
                    stored_seq: 10,
                    ..
                }
            ]
        ),
        "{events:?}"
    );
}

#[test]
fn autosave_reports_permanent_failures_without_retrying() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = Arc::new(SecretRegistry::new());
    secrets.register("abcdefghijklmnop-secret");
    let (mut store, _) = Store::open_dir(dir.path(), Arc::clone(&secrets)).unwrap();
    let mut doc = store.create_function("f").unwrap().document;
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let saver = Autosaver::spawn(
        Store::open_dir(dir.path(), secrets).unwrap().0,
        Duration::from_millis(10),
        move |e| sink.lock().unwrap().push(e),
    );
    doc.description = "abcdefghijklmnop-secret".into();
    saver.submit(doc.clone(), 1);
    assert!(!saver.flush());
    let events = events.lock().unwrap();
    assert!(
        matches!(&events[..], [AutosaveEvent::Failed { will_retry: false, error, .. }] if error.contains("credential")),
        "{events:?}"
    );
    assert!(store.load_draft(doc.id).unwrap().document.description.is_empty());
}

/// Writes drafts and revisions as fast as possible until killed. Run only as a child process of
/// `killed_writers_leave_a_consistent_library`.
#[test]
#[ignore = "helper process for the crash test"]
fn crash_test_writer() {
    let Some(dir) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let mut store = open(Path::new(&dir));
    let existing = store.list_functions(Listing::Active, "").unwrap();
    let (mut doc, mut seq) = match existing.first() {
        Some(f) => {
            let d = store.load_draft(f.id).unwrap();
            (d.document, d.seq)
        }
        None => (store.create_function("crash").unwrap().document, 0),
    };
    let mut announced = false;
    loop {
        seq += 1;
        doc.description = format!("edit {seq}");
        doc.steps = (0..STEPS)
            .map(|k| Step::new(format!("step {k} of edit {seq}")))
            .collect();
        if seq % 4 == 0 {
            store
                .commit_revision(&doc, seq, RevisionReason::new(RevisionKind::Checkpoint))
                .unwrap();
            if !announced {
                println!("{WRITING_MARKER}");
                announced = true;
            }
        } else {
            store.save_draft(&doc, seq).unwrap();
        }
    }
}

/// Every stored document must be internally consistent: all parts from the same edit.
fn assert_consistent(doc: &FunctionDocument) -> u64 {
    let seq: u64 = doc
        .description
        .strip_prefix("edit ")
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("unexpected description {:?}", doc.description));
    assert_eq!(doc.steps.len(), STEPS);
    for (k, step) in doc.steps.iter().enumerate() {
        assert_eq!(step.text, format!("step {k} of edit {seq}"));
    }
    seq
}

#[test]
fn killed_writers_leave_a_consistent_library() {
    let dir = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    let mut last_seq = 0;
    for round in 0..6u64 {
        let mut child = Command::new(&exe)
            .args([
                "--exact",
                "crash_test_writer",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_ENV, dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut lines = BufReader::new(stdout).lines();
            let _ = started_tx.send(lines.any(|l| l.is_ok_and(|l| l.contains(WRITING_MARKER))));
        });
        let started = started_rx.recv_timeout(Duration::from_secs(60));
        if started != Ok(true) {
            let _ = child.kill();
            panic!("writer did not start: {started:?}");
        }
        // Kill at varying points within the write loop.
        std::thread::sleep(Duration::from_millis(20 + round * 37));
        child.kill().unwrap();
        child.wait().unwrap();

        let store = open(dir.path());
        let check: String = store
            .connection()
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(check, "ok");
        let functions = store.list_functions(Listing::Active, "").unwrap();
        assert_eq!(functions.len(), 1);
        let draft = store.load_draft(functions[0].id).unwrap();
        let seq = assert_consistent(&draft.document);
        assert_eq!(seq, draft.seq, "draft row and document come from the same write");
        assert!(seq > last_seq, "every round made progress that survived");
        last_seq = seq;

        let revisions = store.revisions(functions[0].id).unwrap();
        let numbers: Vec<u32> = revisions.iter().rev().map(|r| r.number).collect();
        assert_eq!(
            numbers,
            (1..=u32::try_from(revisions.len()).unwrap()).collect::<Vec<_>>()
        );
        for r in &revisions {
            let rev = store.load_revision(r.id).unwrap();
            assert_consistent(&rev.document);
            assert_eq!(rev.document.content_fingerprint(), r.content);
        }
    }
}
