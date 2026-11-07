//! Local storage for functions: one editable draft per function, an append-only history of
//! immutable revisions, and append-only validation and benchmark results.
//!
//! Durability: SQLite in WAL mode with `synchronous = FULL`. Every operation that touches more
//! than one row runs in a single transaction, so an interrupted write (crash, kill, power loss
//! after the OS acknowledged the write) leaves either the old or the new state, never a mix.
//!
//! Ordering: each draft write carries the editor's monotonic edit sequence. A write with an older
//! sequence than the stored one is ignored, so a delayed autosave can never overwrite a newer
//! draft (for example one produced by restoring a revision).

mod autosave;
pub mod migrate;

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use clojurecraft_domain::format::{self, FormatError, RecordKind};
use clojurecraft_domain::{
    Basis, BenchmarkConfigId, BenchmarkResult, ExampleId, Fingerprint, FunctionDocument, FunctionId,
    RevisionId, RevisionKind, RevisionPin, RevisionReason, SecretRegistry, Timestamp, ValidationReport,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use thiserror::Error;

pub use autosave::{AutosaveEvent, Autosaver};
pub use migrate::MigrationOutcome;

/// File name of the function library inside the library directory.
pub const DATABASE_FILE: &str = "library.sqlite3";

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("storage error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("could not prepare the library directory {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{record}: {source}")]
    Format {
        record: String,
        #[source]
        source: FormatError,
    },
    #[error("could not serialise {record}: {source}")]
    Encode {
        record: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "the library was created by a newer version of ClojureCraft (schema {found}, supported up to {supported}); it was left unchanged"
    )]
    NewerSchema { found: u32, supported: u32 },
    #[error("upgrading the library to schema {version} failed and was rolled back{}: {source}",
        backup.as_ref().map(|b| format!(" (backup at {})", b.display())).unwrap_or_default())]
    Migration {
        version: u32,
        backup: Option<PathBuf>,
        #[source]
        source: rusqlite::Error,
    },
    #[error("library integrity check failed: {0}")]
    Integrity(String),
    #[error("{0} not found")]
    NotFound(String),
    #[error("refusing to store {record}: it contains a credential")]
    SecretDetected { record: &'static str },
    #[error(
        "a newer edit (sequence {stored}) is already stored; this change (sequence {given}) is out of date"
    )]
    StaleSequence { stored: u64, given: u64 },
    #[error("function {0} must be moved to the trash before it can be deleted permanently")]
    NotInTrash(FunctionId),
    #[error("function {function} is still used as an example input by: {}", by.join(", "))]
    Referenced { function: FunctionId, by: Vec<String> },
    #[error("the draft is readable; recovering it from history would discard edits")]
    DraftReadable,
}

impl StoreError {
    /// Whether retrying the same write later may succeed (lock contention, I/O trouble).
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Sqlite(e) => matches!(
                e.sqlite_error_code(),
                Some(
                    rusqlite::ErrorCode::DatabaseBusy
                        | rusqlite::ErrorCode::DatabaseLocked
                        | rusqlite::ErrorCode::DiskFull
                        | rusqlite::ErrorCode::SystemIoFailure
                        | rusqlite::ErrorCode::CannotOpen
                        | rusqlite::ErrorCode::OutOfMemory
                )
            ),
            Self::Io { .. } => true,
            _ => false,
        }
    }
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// A function's editable working copy.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub document: FunctionDocument,
    /// Edit sequence of the stored draft; the editor continues counting from here.
    pub seq: u64,
    pub base_revision: Option<RevisionId>,
    pub updated_at: Timestamp,
    /// Whether the draft differs from the latest revision (or no revision exists yet).
    pub has_unrevisioned_changes: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved,
    /// A newer edit was already stored; nothing was written.
    Superseded {
        stored_seq: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionSummary {
    pub id: FunctionId,
    pub name: String,
    pub description: String,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub deleted_at: Option<Timestamp>,
    pub revision_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listing {
    Active,
    Trash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionSummary {
    pub id: RevisionId,
    pub function: FunctionId,
    pub number: u32,
    pub parent: Option<RevisionId>,
    pub created_at: Timestamp,
    pub reason: RevisionReason,
    pub basis: Basis,
    pub content: Fingerprint,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Revision {
    pub summary: RevisionSummary,
    pub document: FunctionDocument,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitOutcome {
    Created(RevisionSummary),
    /// The draft already equals the latest revision; no revision was added.
    Unchanged(RevisionSummary),
}

impl CommitOutcome {
    pub fn revision(&self) -> &RevisionSummary {
        match self {
            Self::Created(r) | Self::Unchanged(r) => r,
        }
    }
}

/// A function whose examples take input from another function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dependent {
    pub function: FunctionId,
    pub name: String,
}

/// A reference from an example input that cannot currently be resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReferenceProblem {
    MissingFunction {
        example: ExampleId,
        function: FunctionId,
    },
    FunctionInTrash {
        example: ExampleId,
        function: FunctionId,
    },
    /// The upstream function has no saved revision to take output from.
    NoRevisions {
        example: ExampleId,
        function: FunctionId,
    },
    MissingRevision {
        example: ExampleId,
        revision: RevisionId,
    },
    /// The pinned revision belongs to a different function than the one referenced.
    RevisionOfOtherFunction {
        example: ExampleId,
        revision: RevisionId,
    },
    /// The upstream revision has no example with the referenced identifier.
    MissingExample {
        example: ExampleId,
        upstream_example: ExampleId,
    },
}

pub struct Store {
    conn: Connection,
    secrets: Arc<SecretRegistry>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("path", &self.conn.path()).finish()
    }
}

impl Store {
    /// Opens (creating if needed) and migrates the library in `dir`.
    pub fn open_dir(dir: &Path, secrets: Arc<SecretRegistry>) -> Result<(Self, MigrationOutcome)> {
        std::fs::create_dir_all(dir).map_err(|source| StoreError::Io {
            path: dir.to_owned(),
            source,
        })?;
        Self::open(&dir.join(DATABASE_FILE), secrets)
    }

    pub fn open(path: &Path, secrets: Arc<SecretRegistry>) -> Result<(Self, MigrationOutcome)> {
        let mut conn = Connection::open(path)?;
        configure(&conn)?;
        let outcome = migrate::migrate(&mut conn, Some(path), migrate::MIGRATIONS)?;
        Ok((Self { conn, secrets }, outcome))
    }

    pub fn open_in_memory(secrets: Arc<SecretRegistry>) -> Result<Self> {
        let mut conn = Connection::open_in_memory()?;
        configure(&conn)?;
        migrate::migrate(&mut conn, None, migrate::MIGRATIONS)?;
        Ok(Self { conn, secrets })
    }

    pub fn secrets(&self) -> &Arc<SecretRegistry> {
        &self.secrets
    }

    // ---- functions and drafts -------------------------------------------------------------

    pub fn create_function(&mut self, name: &str) -> Result<Draft> {
        self.insert_function(FunctionDocument::new(name))
    }

    /// Stores a complete document as a new function (used for duplication and import).
    pub fn insert_function(&mut self, document: FunctionDocument) -> Result<Draft> {
        let json = self.encode_document(&document, "function draft")?;
        let now = Timestamp::now().0;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO functions (id, name, description, created_at, updated_at, draft_seq,
                                    draft_format, draft, draft_content_fp)
             VALUES (?1, ?2, ?3, ?4, ?4, 0, ?5, ?6, ?7)",
            params![
                document.id.to_string(),
                document.name,
                document.description,
                now,
                RecordKind::Document.current(),
                json,
                document.content_fingerprint().as_str(),
            ],
        )?;
        index_references(&tx, &document, None)?;
        tx.commit()?;
        self.load_draft(document.id)
    }

    pub fn load_draft(&self, id: FunctionId) -> Result<Draft> {
        let row = self
            .conn
            .query_row(
                "SELECT draft_seq, draft_format, draft, draft_content_fp, base_revision_id, updated_at
                 FROM functions WHERE id = ?1",
                [id.to_string()],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, u32>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("function {id}")))?;
        let (seq, fmt, json, content_fp, base, updated_at) = row;
        let document: FunctionDocument =
            format::decode(RecordKind::Document, fmt, &json).map_err(|source| StoreError::Format {
                record: format!("draft of function {id}"),
                source,
            })?;
        if document.id != id {
            return Err(StoreError::Integrity(format!(
                "draft of function {id} has identity {}",
                document.id
            )));
        }
        let head = head_revision_fp(&self.conn, id)?;
        Ok(Draft {
            document,
            seq: to_u64(seq),
            base_revision: base.map(parse_id).transpose()?,
            updated_at: Timestamp(updated_at),
            has_unrevisioned_changes: head.as_deref() != Some(content_fp.as_str()),
        })
    }

    /// Writes the draft unless a newer edit sequence is already stored.
    pub fn save_draft(&mut self, document: &FunctionDocument, seq: u64) -> Result<SaveOutcome> {
        let json = self.encode_document(document, "function draft")?;
        let tx = self.conn.transaction()?;
        let outcome = write_draft(&tx, document, &json, seq, None)?;
        tx.commit()?;
        Ok(outcome)
    }

    pub fn list_functions(&self, listing: Listing, search: &str) -> Result<Vec<FunctionSummary>> {
        let pattern = format!("%{}%", escape_like(search.trim()));
        let deleted = match listing {
            Listing::Active => "f.deleted_at IS NULL",
            Listing::Trash => "f.deleted_at IS NOT NULL",
        };
        let sql = format!(
            "SELECT f.id, f.name, f.description, f.created_at, f.updated_at, f.deleted_at,
                    (SELECT count(*) FROM revisions r WHERE r.function_id = f.id)
             FROM functions f
             WHERE {deleted}
               AND (f.name LIKE ?1 ESCAPE '\\' OR f.description LIKE ?1 ESCAPE '\\')
             ORDER BY f.updated_at DESC, f.name"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([pattern], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<i64>>(5)?,
                r.get::<_, u32>(6)?,
            ))
        })?;
        rows.map(|row| {
            let (id, name, description, created, updated, deleted, revision_count) = row?;
            Ok(FunctionSummary {
                id: parse_id(id)?,
                name,
                description,
                created_at: Timestamp(created),
                updated_at: Timestamp(updated),
                deleted_at: deleted.map(Timestamp),
                revision_count,
            })
        })
        .collect()
    }

    /// Copies the current draft into a new function with fresh identity and no history.
    pub fn duplicate_function(&mut self, id: FunctionId, name: &str) -> Result<Draft> {
        let mut document = self.load_draft(id)?.document;
        document.id = FunctionId::generate();
        name.clone_into(&mut document.name);
        self.insert_function(document)
    }

    /// Moves a function to the trash. It keeps its history and can be restored.
    pub fn trash_function(&mut self, id: FunctionId) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE functions SET deleted_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![id.to_string(), Timestamp::now().0],
        )?;
        if n == 0 {
            self.require_function(id)?;
        }
        Ok(())
    }

    pub fn restore_function(&mut self, id: FunctionId) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE functions SET deleted_at = NULL WHERE id = ?1",
            [id.to_string()],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound(format!("function {id}")));
        }
        Ok(())
    }

    /// Permanently deletes a trashed function with all its history and results. Refused while
    /// any other function (draft or revision) takes example input from it.
    pub fn purge_function(&mut self, id: FunctionId) -> Result<()> {
        let tx = self.conn.transaction()?;
        let deleted: Option<Option<i64>> = tx
            .query_row(
                "SELECT deleted_at FROM functions WHERE id = ?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        match deleted {
            None => return Err(StoreError::NotFound(format!("function {id}"))),
            Some(None) => return Err(StoreError::NotInTrash(id)),
            Some(Some(_)) => {}
        }
        let dependents = dependents(&tx, id)?;
        if !dependents.is_empty() {
            return Err(StoreError::Referenced {
                function: id,
                by: dependents.into_iter().map(|d| d.name).collect(),
            });
        }
        tx.execute("DELETE FROM functions WHERE id = ?1", [id.to_string()])?;
        tx.commit()?;
        Ok(())
    }

    /// Other functions whose draft or history takes example input from `id`.
    pub fn dependents(&self, id: FunctionId) -> Result<Vec<Dependent>> {
        dependents(&self.conn, id)
    }

    /// Replaces an unreadable draft with the latest revision. Refused when the draft can still be
    /// read, or was written by a newer application version, so no edits are discarded.
    pub fn recover_draft(&mut self, id: FunctionId) -> Result<Draft> {
        match self.load_draft(id) {
            Ok(_) => return Err(StoreError::DraftReadable),
            Err(
                StoreError::Format {
                    source: FormatError::Malformed { .. },
                    ..
                }
                | StoreError::Integrity(_),
            ) => {}
            Err(e) => return Err(e),
        }
        let head =
            self.revisions(id)?.into_iter().next().ok_or_else(|| {
                StoreError::NotFound(format!("a revision of function {id} to recover from"))
            })?;
        let revision = self.load_revision(head.id)?;
        let json = self.encode_document(&revision.document, "function draft")?;
        let tx = self.conn.transaction()?;
        let stored: i64 = tx.query_row(
            "SELECT draft_seq FROM functions WHERE id = ?1",
            [id.to_string()],
            |r| r.get(0),
        )?;
        write_draft(
            &tx,
            &revision.document,
            &json,
            to_u64(stored) + 1,
            Some(Some(head.id)),
        )?;
        tx.commit()?;
        self.load_draft(id)
    }

    // ---- revisions ------------------------------------------------------------------------

    /// Saves the draft and records it as a new immutable revision, unless it equals the latest one.
    pub fn commit_revision(
        &mut self,
        document: &FunctionDocument,
        seq: u64,
        reason: RevisionReason,
    ) -> Result<CommitOutcome> {
        let json = self.encode_document(document, "function revision")?;
        let reason_json = encode(&reason, "revision reason")?;
        let tx = self.conn.transaction()?;
        reject_stale(&tx, document.id, seq)?;
        let head = head_revision(&tx, document.id)?;
        let outcome = match head {
            Some(head) if head.content == document.content_fingerprint() => CommitOutcome::Unchanged(head),
            head => CommitOutcome::Created(insert_revision(
                &tx,
                document,
                &json,
                &reason,
                &reason_json,
                head.as_ref(),
            )?),
        };
        write_draft(&tx, document, &json, seq, Some(Some(outcome.revision().id)))?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Newest first.
    pub fn revisions(&self, function: FunctionId) -> Result<Vec<RevisionSummary>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {REVISION_COLUMNS} FROM revisions WHERE function_id = ?1 ORDER BY number DESC"
        ))?;
        let rows = stmt.query_map([function.to_string()], revision_row)?;
        rows.map(|r| r?.into_summary()).collect()
    }

    pub fn load_revision(&self, id: RevisionId) -> Result<Revision> {
        let (row, fmt, json) = self
            .conn
            .query_row(
                &format!("SELECT {REVISION_COLUMNS}, format, document FROM revisions WHERE id = ?1"),
                [id.to_string()],
                |r| Ok((revision_row(r)?, r.get::<_, u32>(11)?, r.get::<_, String>(12)?)),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound(format!("revision {id}")))?;
        let summary = row.into_summary()?;
        let document =
            format::decode(RecordKind::Document, fmt, &json).map_err(|source| StoreError::Format {
                record: format!("revision {} of function {}", summary.number, summary.function),
                source,
            })?;
        Ok(Revision { summary, document })
    }

    /// Makes an older revision the current content without deleting any history: the current
    /// draft is first recorded (if it has unrevisioned changes), then the restored content is
    /// appended as the newest revision and becomes the draft.
    pub fn restore_revision(
        &mut self,
        function: FunctionId,
        revision: RevisionId,
        seq: u64,
    ) -> Result<Draft> {
        let target = self.load_revision(revision)?;
        if target.summary.function != function {
            return Err(StoreError::NotFound(format!(
                "revision {revision} of function {function}"
            )));
        }
        let current = self.load_draft(function)?;
        let current_json = self.encode_document(&current.document, "function revision")?;
        let target_json = self.encode_document(&target.document, "function revision")?;
        let tx = self.conn.transaction()?;
        reject_stale(&tx, function, seq)?;
        let mut head = head_revision(&tx, function)?;
        if head
            .as_ref()
            .is_none_or(|h| h.content != current.document.content_fingerprint())
        {
            let reason = RevisionReason::new(RevisionKind::BeforeRestore);
            let reason_json = encode(&reason, "revision reason")?;
            head = Some(insert_revision(
                &tx,
                &current.document,
                &current_json,
                &reason,
                &reason_json,
                head.as_ref(),
            )?);
        }
        if head.as_ref().is_none_or(|h| h.content != target.summary.content) {
            let reason = RevisionReason::new(RevisionKind::Restored {
                from: revision,
                from_number: target.summary.number,
            });
            let reason_json = encode(&reason, "revision reason")?;
            head = Some(insert_revision(
                &tx,
                &target.document,
                &target_json,
                &reason,
                &reason_json,
                head.as_ref(),
            )?);
        }
        write_draft(&tx, &target.document, &target_json, seq, Some(head.map(|h| h.id)))?;
        tx.commit()?;
        self.load_draft(function)
    }

    // ---- results --------------------------------------------------------------------------

    /// Appends a validation report. Credentials in machine-produced text are redacted first; the
    /// stored (redacted) report is returned.
    pub fn record_validation(
        &mut self,
        function: FunctionId,
        report: &ValidationReport,
    ) -> Result<ValidationReport> {
        let report = self
            .secrets
            .redact_record(report)
            .map_err(|source| StoreError::Encode {
                record: "validation report",
                source,
            })?;
        let json = encode(&report, "validation report")?;
        self.conn.execute(
            "INSERT INTO validation_reports (id, function_id, created_at, spec_fp, code_fp, tests_fp,
                                             format, report)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                report.id.to_string(),
                function.to_string(),
                report.created_at.0,
                report.basis.spec.as_str(),
                report.basis.code.as_str(),
                report.basis.tests.as_str(),
                RecordKind::ValidationReport.current(),
                json,
            ],
        )?;
        Ok(report)
    }

    /// All reports for a function, newest first, current or stale.
    pub fn validation_reports(&self, function: FunctionId) -> Result<Vec<ValidationReport>> {
        self.query_reports(
            "WHERE function_id = ?1 ORDER BY created_at DESC, rowid DESC",
            &[&function.to_string()],
        )
    }

    /// Reports computed from exactly this content, newest first (for example a revision's status).
    pub fn validation_for(&self, function: FunctionId, basis: &Basis) -> Result<Vec<ValidationReport>> {
        self.query_reports(
            "WHERE function_id = ?1 AND spec_fp = ?2 AND code_fp = ?3 AND tests_fp = ?4
             ORDER BY created_at DESC, rowid DESC",
            &[
                &function.to_string(),
                &basis.spec.as_str(),
                &basis.code.as_str(),
                &basis.tests.as_str(),
            ],
        )
    }

    fn query_reports(&self, clause: &str, params: &[&dyn rusqlite::ToSql]) -> Result<Vec<ValidationReport>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT format, report FROM validation_reports {clause}"))?;
        let rows = stmt.query_map(params, |r| Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|row| {
            let (fmt, json) = row?;
            format::decode(RecordKind::ValidationReport, fmt, &json).map_err(|source| StoreError::Format {
                record: "validation report".into(),
                source,
            })
        })
        .collect()
    }

    pub fn record_benchmark(
        &mut self,
        function: FunctionId,
        result: &BenchmarkResult,
    ) -> Result<BenchmarkResult> {
        let result = self
            .secrets
            .redact_record(result)
            .map_err(|source| StoreError::Encode {
                record: "benchmark result",
                source,
            })?;
        let json = encode(&result, "benchmark result")?;
        self.conn.execute(
            "INSERT INTO benchmark_results (id, function_id, config_id, created_at, format, result)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                result.id.to_string(),
                function.to_string(),
                result.config_id.to_string(),
                result.created_at.0,
                RecordKind::BenchmarkResult.current(),
                json,
            ],
        )?;
        Ok(result)
    }

    /// Newest first; optionally only for one configuration.
    pub fn benchmark_results(
        &self,
        function: FunctionId,
        config: Option<BenchmarkConfigId>,
    ) -> Result<Vec<BenchmarkResult>> {
        let mut stmt = self.conn.prepare(
            "SELECT format, result FROM benchmark_results
             WHERE function_id = ?1 AND (?2 IS NULL OR config_id = ?2)
             ORDER BY created_at DESC, rowid DESC",
        )?;
        let rows = stmt.query_map(
            params![function.to_string(), config.map(|c| c.to_string())],
            |r| Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?)),
        )?;
        rows.map(|row| {
            let (fmt, json) = row?;
            format::decode(RecordKind::BenchmarkResult, fmt, &json).map_err(|source| StoreError::Format {
                record: "benchmark result".into(),
                source,
            })
        })
        .collect()
    }

    // ---- references -----------------------------------------------------------------------

    /// Checks every example input of the function's draft that references another function.
    pub fn check_references(&self, function: FunctionId) -> Result<Vec<ReferenceProblem>> {
        let draft = self.load_draft(function)?;
        self.check_document_references(&draft.document)
    }

    /// Checks every referenced example input of `document`, which may be newer than its stored
    /// draft (for example an editor's copy that is still waiting to be saved).
    pub fn check_document_references(&self, document: &FunctionDocument) -> Result<Vec<ReferenceProblem>> {
        let mut problems = Vec::new();
        for (example, reference) in document.output_references() {
            let target = reference.function;
            let deleted: Option<Option<i64>> = self
                .conn
                .query_row(
                    "SELECT deleted_at FROM functions WHERE id = ?1",
                    [target.to_string()],
                    |r| r.get(0),
                )
                .optional()?;
            match deleted {
                None => {
                    problems.push(ReferenceProblem::MissingFunction {
                        example,
                        function: target,
                    });
                    continue;
                }
                Some(Some(_)) => problems.push(ReferenceProblem::FunctionInTrash {
                    example,
                    function: target,
                }),
                Some(None) => {}
            }
            let revision = match reference.revision {
                RevisionPin::Pinned(id) => match self.load_revision(id) {
                    Ok(r) if r.summary.function == target => r,
                    Ok(_) => {
                        problems.push(ReferenceProblem::RevisionOfOtherFunction {
                            example,
                            revision: id,
                        });
                        continue;
                    }
                    Err(StoreError::NotFound(_)) => {
                        problems.push(ReferenceProblem::MissingRevision {
                            example,
                            revision: id,
                        });
                        continue;
                    }
                    Err(e) => return Err(e),
                },
                RevisionPin::Latest => match head_revision(&self.conn, target)? {
                    Some(head) => self.load_revision(head.id)?,
                    None => {
                        problems.push(ReferenceProblem::NoRevisions {
                            example,
                            function: target,
                        });
                        continue;
                    }
                },
            };
            if revision.document.example(reference.example).is_none() {
                problems.push(ReferenceProblem::MissingExample {
                    example,
                    upstream_example: reference.example,
                });
            }
        }
        Ok(problems)
    }

    /// Every reference from a function's current draft to another function, as (referencing,
    /// referenced) pairs, for finding dependency cycles. Functions in the trash are included,
    /// because they can be restored with their references.
    pub fn reference_edges(&self) -> Result<Vec<(FunctionId, FunctionId)>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT source_function, target_function FROM output_references
             WHERE source_revision IS NULL",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.map(|row| {
            let (from, to) = row?;
            Ok((parse_id(from)?, parse_id(to)?))
        })
        .collect()
    }

    // ---- helpers --------------------------------------------------------------------------

    fn require_function(&self, id: FunctionId) -> Result<()> {
        let exists: Option<i64> = self
            .conn
            .query_row("SELECT 1 FROM functions WHERE id = ?1", [id.to_string()], |r| {
                r.get(0)
            })
            .optional()?;
        exists
            .map(|_| ())
            .ok_or_else(|| StoreError::NotFound(format!("function {id}")))
    }

    /// Encodes user content, refusing it if it contains a registered credential.
    fn encode_document(&self, document: &FunctionDocument, record: &'static str) -> Result<String> {
        let json = encode(document, record)?;
        if self.secrets.contains_secret(&json) {
            return Err(StoreError::SecretDetected { record });
        }
        Ok(json)
    }

    #[doc(hidden)]
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") && !mode.eq_ignore_ascii_case("memory") {
        tracing::warn!(%mode, "write-ahead logging unavailable; using the default journal");
    }
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(StoreError::Integrity(check));
    }
    Ok(())
}

fn encode<T: serde::Serialize>(record: &T, label: &'static str) -> Result<String> {
    format::encode(record).map_err(|source| StoreError::Encode {
        record: label,
        source,
    })
}

fn parse_id<T: FromStr>(s: String) -> Result<T> {
    s.parse()
        .map_err(|_| StoreError::Integrity(format!("malformed identifier {s:?}")))
}

fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn reject_stale(tx: &Transaction<'_>, function: FunctionId, seq: u64) -> Result<()> {
    let stored: Option<i64> = tx
        .query_row(
            "SELECT draft_seq FROM functions WHERE id = ?1",
            [function.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    match stored.map(to_u64) {
        None => Err(StoreError::NotFound(format!("function {function}"))),
        Some(stored) if stored > seq => Err(StoreError::StaleSequence { stored, given: seq }),
        Some(_) => Ok(()),
    }
}

/// `base_revision`: `None` keeps the stored value, `Some(v)` sets it.
fn write_draft(
    tx: &Transaction<'_>,
    document: &FunctionDocument,
    json: &str,
    seq: u64,
    base_revision: Option<Option<RevisionId>>,
) -> Result<SaveOutcome> {
    let id = document.id.to_string();
    let n = tx.execute(
        "UPDATE functions
         SET name = ?2, description = ?3, updated_at = ?4, draft_seq = ?5, draft_format = ?6,
             draft = ?7, draft_content_fp = ?8,
             base_revision_id = CASE WHEN ?9 THEN ?10 ELSE base_revision_id END
         WHERE id = ?1 AND draft_seq <= ?5",
        params![
            id,
            document.name,
            document.description,
            Timestamp::now().0,
            to_i64(seq),
            RecordKind::Document.current(),
            json,
            document.content_fingerprint().as_str(),
            base_revision.is_some(),
            base_revision.flatten().map(|r| r.to_string()),
        ],
    )?;
    if n == 0 {
        let stored: Option<i64> = tx
            .query_row("SELECT draft_seq FROM functions WHERE id = ?1", [&id], |r| {
                r.get(0)
            })
            .optional()?;
        return match stored {
            None => Err(StoreError::NotFound(format!("function {id}"))),
            Some(s) => Ok(SaveOutcome::Superseded {
                stored_seq: to_u64(s),
            }),
        };
    }
    index_references(tx, document, None)?;
    Ok(SaveOutcome::Saved)
}

fn index_references(
    conn: &Connection,
    document: &FunctionDocument,
    revision: Option<RevisionId>,
) -> Result<()> {
    let source = document.id.to_string();
    let revision = revision.map(|r| r.to_string());
    conn.execute(
        "DELETE FROM output_references WHERE source_function = ?1 AND source_revision IS ?2",
        params![source, revision],
    )?;
    let mut insert = conn.prepare_cached(
        "INSERT INTO output_references (source_function, source_revision, example_id,
                                        target_function, target_revision, target_example)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for (example, reference) in document.output_references() {
        let pinned = match reference.revision {
            RevisionPin::Pinned(r) => Some(r.to_string()),
            RevisionPin::Latest => None,
        };
        insert.execute(params![
            source,
            revision,
            example.to_string(),
            reference.function.to_string(),
            pinned,
            reference.example.to_string(),
        ])?;
    }
    Ok(())
}

fn dependents(conn: &Connection, id: FunctionId) -> Result<Vec<Dependent>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT f.id, f.name FROM output_references r
         JOIN functions f ON f.id = r.source_function
         WHERE r.target_function = ?1 AND r.source_function <> ?1
         ORDER BY f.name",
    )?;
    let rows = stmt.query_map([id.to_string()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (function, name) = row?;
        Ok(Dependent {
            function: parse_id(function)?,
            name,
        })
    })
    .collect()
}

const REVISION_COLUMNS: &str = "id, function_id, number, parent_id, created_at, reason_format, reason, content_fp, spec_fp, code_fp, tests_fp";

struct RevisionRow {
    id: String,
    function: String,
    number: u32,
    parent: Option<String>,
    created_at: i64,
    reason_format: u32,
    reason: String,
    content: String,
    spec: String,
    code: String,
    tests: String,
}

fn revision_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RevisionRow> {
    Ok(RevisionRow {
        id: r.get(0)?,
        function: r.get(1)?,
        number: r.get(2)?,
        parent: r.get(3)?,
        created_at: r.get(4)?,
        reason_format: r.get(5)?,
        reason: r.get(6)?,
        content: r.get(7)?,
        spec: r.get(8)?,
        code: r.get(9)?,
        tests: r.get(10)?,
    })
}

impl RevisionRow {
    fn into_summary(self) -> Result<RevisionSummary> {
        let reason = format::decode(RecordKind::RevisionReason, self.reason_format, &self.reason).map_err(
            |source| StoreError::Format {
                record: format!("reason of revision {}", self.id),
                source,
            },
        )?;
        Ok(RevisionSummary {
            id: parse_id(self.id)?,
            function: parse_id(self.function)?,
            number: self.number,
            parent: self.parent.map(parse_id).transpose()?,
            created_at: Timestamp(self.created_at),
            reason,
            basis: Basis {
                spec: Fingerprint::from_stored(self.spec),
                code: Fingerprint::from_stored(self.code),
                tests: Fingerprint::from_stored(self.tests),
            },
            content: Fingerprint::from_stored(self.content),
        })
    }
}

fn head_revision(conn: &Connection, function: FunctionId) -> Result<Option<RevisionSummary>> {
    conn.query_row(
        &format!(
            "SELECT {REVISION_COLUMNS} FROM revisions WHERE function_id = ?1 ORDER BY number DESC LIMIT 1"
        ),
        [function.to_string()],
        revision_row,
    )
    .optional()?
    .map(RevisionRow::into_summary)
    .transpose()
}

fn head_revision_fp(conn: &Connection, function: FunctionId) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT content_fp FROM revisions WHERE function_id = ?1 ORDER BY number DESC LIMIT 1",
            [function.to_string()],
            |r| r.get(0),
        )
        .optional()?)
}

fn insert_revision(
    tx: &Transaction<'_>,
    document: &FunctionDocument,
    json: &str,
    reason: &RevisionReason,
    reason_json: &str,
    parent: Option<&RevisionSummary>,
) -> Result<RevisionSummary> {
    let summary = RevisionSummary {
        id: RevisionId::generate(),
        function: document.id,
        number: parent.map_or(1, |p| p.number + 1),
        parent: parent.map(|p| p.id),
        created_at: Timestamp::now(),
        reason: reason.clone(),
        basis: document.basis(),
        content: document.content_fingerprint(),
    };
    tx.execute(
        "INSERT INTO revisions (id, function_id, number, parent_id, created_at, reason_format, reason,
                                format, document, content_fp, spec_fp, code_fp, tests_fp)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            summary.id.to_string(),
            summary.function.to_string(),
            summary.number,
            summary.parent.map(|p| p.to_string()),
            summary.created_at.0,
            RecordKind::RevisionReason.current(),
            reason_json,
            RecordKind::Document.current(),
            json,
            summary.content.as_str(),
            summary.basis.spec.as_str(),
            summary.basis.code.as_str(),
            summary.basis.tests.as_str(),
        ],
    )?;
    index_references(tx, document, Some(summary.id))?;
    Ok(summary)
}
