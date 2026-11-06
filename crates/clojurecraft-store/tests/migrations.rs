//! Schema migration contracts: data is preserved, failures roll back, and newer databases are
//! never modified.

// Test helpers outside #[test] functions fail loudly on setup errors by design.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use clojurecraft_domain::SecretRegistry;
use clojurecraft_store::migrate::{MIGRATIONS, Migration, migrate, schema_version};
use clojurecraft_store::{DATABASE_FILE, Listing, Store, StoreError};
use rusqlite::Connection;

const V1: Migration = Migration {
    version: 1,
    sql: "CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT NOT NULL);",
};
const V2: Migration = Migration {
    version: 2,
    sql: "ALTER TABLE notes ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;",
};
const V3_BROKEN: Migration = Migration {
    version: 3,
    sql: "CREATE TABLE tags (id INTEGER PRIMARY KEY); INSERT INTO missing_table VALUES (1);",
};

#[test]
fn upgrades_preserve_data_and_keep_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite3");
    let mut conn = Connection::open(&path).unwrap();
    let fresh = migrate(&mut conn, Some(&path), &[V1]).unwrap();
    assert_eq!((fresh.from, fresh.to, fresh.backup), (0, 1, None));
    conn.execute("INSERT INTO notes (body) VALUES ('kept')", [])
        .unwrap();

    let upgraded = migrate(&mut conn, Some(&path), &[V1, V2]).unwrap();
    assert_eq!((upgraded.from, upgraded.to), (1, 2));
    let backup = upgraded.backup.expect("existing databases are backed up");
    let (body, pinned): (String, i64) = conn
        .query_row("SELECT body, pinned FROM notes", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((body.as_str(), pinned), ("kept", 0));

    let old = Connection::open(&backup).unwrap();
    assert_eq!(schema_version(&old).unwrap(), 1);
    let n: i64 = old
        .query_row("SELECT count(*) FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn a_failing_migration_rolls_back_completely() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite3");
    let mut conn = Connection::open(&path).unwrap();
    migrate(&mut conn, Some(&path), &[V1, V2]).unwrap();
    conn.execute("INSERT INTO notes (body) VALUES ('kept')", [])
        .unwrap();

    let err = migrate(&mut conn, Some(&path), &[V1, V2, V3_BROKEN]).unwrap_err();
    let StoreError::Migration {
        version: 3,
        backup: Some(backup),
        ..
    } = &err
    else {
        panic!("unexpected error: {err}");
    };
    assert!(backup.exists());
    assert_eq!(schema_version(&conn).unwrap(), 2);
    let tags: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'tags'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tags, 0, "partial schema changes are rolled back");
    let body: String = conn
        .query_row("SELECT body FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(body, "kept");
}

#[test]
fn a_newer_library_is_refused_and_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DATABASE_FILE);
    {
        let (mut store, _) = Store::open(&path, Arc::new(SecretRegistry::new())).unwrap();
        store.create_function("from today").unwrap();
    }
    let future = MIGRATIONS.last().unwrap().version + 1;
    Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", future)
        .unwrap();

    let err = Store::open(&path, Arc::new(SecretRegistry::new())).unwrap_err();
    assert!(
        matches!(err, StoreError::NewerSchema { found, .. } if found == future),
        "{err}"
    );
    let conn = Connection::open(&path).unwrap();
    assert_eq!(schema_version(&conn).unwrap(), future);
    let n: i64 = conn
        .query_row("SELECT count(*) FROM functions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "bak"))
            .count(),
        0,
        "no backup or other write for a refused database"
    );
}

#[test]
fn the_shipped_schema_applies_to_an_empty_library() {
    let dir = tempfile::tempdir().unwrap();
    let (store, outcome) = Store::open_dir(dir.path(), Arc::new(SecretRegistry::new())).unwrap();
    assert_eq!(outcome.to, MIGRATIONS.last().unwrap().version);
    assert!(store.list_functions(Listing::Active, "").unwrap().is_empty());
    let (_, again) = Store::open_dir(dir.path(), Arc::new(SecretRegistry::new())).unwrap();
    assert_eq!(
        (again.from, again.to, again.backup),
        (outcome.to, outcome.to, None)
    );
}
