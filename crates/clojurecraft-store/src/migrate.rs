//! Schema migrations, tracked in SQLite's `user_version`.
//!
//! Before an existing database is migrated, a consistent copy is written next to it. Each
//! migration runs in its own transaction together with its version bump, so a failure leaves the
//! database at the last fully applied version. A database newer than this application is never
//! touched.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::StoreError;

pub struct Migration {
    pub version: u32,
    pub sql: &'static str,
}

pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: include_str!("migrations/0001_initial.sql"),
}];

#[derive(Debug, Default, PartialEq, Eq)]
pub struct MigrationOutcome {
    pub from: u32,
    pub to: u32,
    /// Copy of the database taken before migrating, if one was needed.
    pub backup: Option<PathBuf>,
}

pub fn schema_version(conn: &Connection) -> Result<u32, StoreError> {
    Ok(conn.pragma_query_value(None, "user_version", |r| r.get(0))?)
}

pub fn migrate(
    conn: &mut Connection,
    db_path: Option<&Path>,
    migrations: &[Migration],
) -> Result<MigrationOutcome, StoreError> {
    let from = schema_version(conn)?;
    let latest = migrations.last().map_or(0, |m| m.version);
    if from > latest {
        return Err(StoreError::NewerSchema {
            found: from,
            supported: latest,
        });
    }
    let mut outcome = MigrationOutcome {
        from,
        to: from,
        backup: None,
    };
    if from == latest {
        return Ok(outcome);
    }
    if from > 0
        && let Some(path) = db_path
    {
        outcome.backup = Some(backup(conn, path, from)?);
    }
    for m in migrations.iter().filter(|m| m.version > from) {
        let tx = conn.transaction()?;
        tx.execute_batch(m.sql)
            .and_then(|()| tx.pragma_update(None, "user_version", m.version))
            .and_then(|()| tx.commit())
            .map_err(|source| StoreError::Migration {
                version: m.version,
                backup: outcome.backup.clone(),
                source,
            })?;
        outcome.to = m.version;
        tracing::info!(version = m.version, "applied storage migration");
    }
    let violations: i64 =
        conn.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0))?;
    if violations > 0 {
        return Err(StoreError::Integrity(format!(
            "{violations} foreign-key violations after migrating to version {}",
            outcome.to
        )));
    }
    Ok(outcome)
}

fn backup(conn: &Connection, db_path: &Path, version: u32) -> Result<PathBuf, StoreError> {
    let file_name = db_path
        .file_name()
        .map_or_else(|| "library.sqlite3".into(), |n| n.to_string_lossy().into_owned());
    let mut target = db_path.with_file_name(format!("{file_name}.v{version}.bak"));
    let mut n = 1;
    while target.exists() {
        n += 1;
        target = db_path.with_file_name(format!("{file_name}.v{version}.{n}.bak"));
    }
    conn.execute("VACUUM INTO ?1", [target.to_string_lossy()])?;
    Ok(target)
}
