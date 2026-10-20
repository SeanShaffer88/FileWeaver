//! Durable local operation ledger.
//!
//! SQLite is used as an embedded, transactional implementation detail. The
//! journal favors recoverability over hand editing. It records what was
//! applied so a job can be reviewed against the current state of the disk
//! and rolled back conservatively.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;

use crate::domain::{FileFingerprint, PlannedJob};

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("could not use the local operation ledger: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("could not create the operation ledger directory: {0}")]
    CreateDirectory(#[source] std::io::Error),
    #[error("could not serialize job information: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("job {0} was not found")]
    JobNotFound(i64),
}

#[derive(Debug)]
pub struct Journal {
    connection: Connection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedItem {
    pub index: usize,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub result_fingerprint: FileFingerprint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackBlockedItem {
    pub index: usize,
    pub destination: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RollbackReport {
    pub restored: usize,
    pub removed_copies: usize,
    pub skipped: Vec<RollbackBlockedItem>,
}

/// A read-only summary of one recorded job, for history and recovery views.
#[derive(Debug, Clone)]
pub struct JobSummary {
    pub id: i64,
    pub started_unix_ms: i64,
    pub completed_unix_ms: Option<i64>,
    pub status: String,
    pub message: Option<String>,
    pub operation: crate::domain::OperationKind,
    pub total_items: usize,
    pub applied_items: usize,
    pub failed_items: usize,
    pub rolled_back_items: usize,
}

impl Journal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(JournalError::CreateDirectory)?;
        }
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS jobs (
                id INTEGER PRIMARY KEY,
                started_unix_ms INTEGER NOT NULL,
                completed_unix_ms INTEGER,
                status TEXT NOT NULL,
                request_json TEXT NOT NULL,
                pipeline_json TEXT NOT NULL,
                message TEXT
            );
            CREATE TABLE IF NOT EXISTS job_items (
                job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
                item_index INTEGER NOT NULL,
                source_path TEXT NOT NULL,
                destination_path TEXT NOT NULL,
                temp_path TEXT,
                status TEXT NOT NULL,
                error_message TEXT,
                source_size INTEGER NOT NULL,
                source_modified_ns TEXT,
                result_size INTEGER,
                result_modified_ns TEXT,
                PRIMARY KEY(job_id, item_index)
            );
            CREATE INDEX IF NOT EXISTS job_items_status ON job_items(job_id, status);
            ",
        )?;
        Ok(Self { connection })
    }

    pub fn create_job(&mut self, plan: &PlannedJob) -> Result<i64, JournalError> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO jobs (started_unix_ms, status, request_json, pipeline_json)
             VALUES (?1, 'running', ?2, ?3)",
            params![
                now_unix_ms(),
                serde_json::to_string(&plan.request)?,
                plan.pipeline_json,
            ],
        )?;
        let job_id = transaction.last_insert_rowid();
        for action in plan
            .actions
            .iter()
            .filter(|action| action.will_change() && !action.is_blocked())
        {
            transaction.execute(
                "INSERT INTO job_items (
                    job_id, item_index, source_path, destination_path, status,
                    source_size, source_modified_ns
                ) VALUES (?1, ?2, ?3, ?4, 'planned', ?5, ?6)",
                params![
                    job_id,
                    i64::try_from(action.index).unwrap_or(i64::MAX),
                    path_text(&action.snapshot.source),
                    path_text(&action.destination),
                    i64::try_from(action.snapshot.fingerprint.size_bytes).unwrap_or(i64::MAX),
                    action
                        .snapshot
                        .fingerprint
                        .modified_unix_nanos
                        .map(|value| value.to_string()),
                ],
            )?;
        }
        transaction.commit()?;
        Ok(job_id)
    }

    pub fn mark_staged(&self, job_id: i64, index: usize, temp: &Path) -> Result<(), JournalError> {
        self.connection.execute(
            "UPDATE job_items SET status = 'staged', temp_path = ?1
             WHERE job_id = ?2 AND item_index = ?3",
            params![
                path_text(temp),
                job_id,
                i64::try_from(index).unwrap_or(i64::MAX)
            ],
        )?;
        Ok(())
    }

    pub fn mark_applied(
        &self,
        job_id: i64,
        index: usize,
        fingerprint: &FileFingerprint,
    ) -> Result<(), JournalError> {
        self.connection.execute(
            "UPDATE job_items SET status = 'applied', result_size = ?1,
             result_modified_ns = ?2, error_message = NULL WHERE job_id = ?3 AND item_index = ?4",
            params![
                i64::try_from(fingerprint.size_bytes).unwrap_or(i64::MAX),
                fingerprint
                    .modified_unix_nanos
                    .map(|value| value.to_string()),
                job_id,
                i64::try_from(index).unwrap_or(i64::MAX),
            ],
        )?;
        Ok(())
    }

    pub fn mark_failed(
        &self,
        job_id: i64,
        index: usize,
        message: &str,
    ) -> Result<(), JournalError> {
        self.connection.execute(
            "UPDATE job_items SET status = 'failed', error_message = ?1 WHERE job_id = ?2 AND item_index = ?3",
            params![message, job_id, i64::try_from(index).unwrap_or(i64::MAX)],
        )?;
        Ok(())
    }

    pub fn finish_job(
        &self,
        job_id: i64,
        status: &str,
        message: Option<&str>,
    ) -> Result<(), JournalError> {
        self.connection.execute(
            "UPDATE jobs SET completed_unix_ms = ?1, status = ?2, message = ?3 WHERE id = ?4",
            params![now_unix_ms(), status, message, job_id],
        )?;
        Ok(())
    }

    pub fn applied_items(&self, job_id: i64) -> Result<Vec<AppliedItem>, JournalError> {
        let exists: Option<i64> = self
            .connection
            .query_row(
                "SELECT id FROM jobs WHERE id = ?1",
                params![job_id],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(JournalError::JobNotFound(job_id));
        }

        let mut statement = self.connection.prepare(
            "SELECT item_index, source_path, destination_path, result_size, result_modified_ns
             FROM job_items WHERE job_id = ?1 AND status = 'applied' ORDER BY item_index DESC",
        )?;
        let items = statement
            .query_map(params![job_id], |row| {
                let size: i64 = row.get(3)?;
                let modified: Option<String> = row.get(4)?;
                Ok(AppliedItem {
                    index: usize::try_from(row.get::<_, i64>(0)?).unwrap_or(usize::MAX),
                    source: PathBuf::from(row.get::<_, String>(1)?),
                    destination: PathBuf::from(row.get::<_, String>(2)?),
                    result_fingerprint: FileFingerprint {
                        size_bytes: u64::try_from(size).unwrap_or(0),
                        modified_unix_nanos: modified.and_then(|value| value.parse().ok()),
                    },
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(items)
    }

    /// Most recent jobs first, with per-status item counts, for history and
    /// recovery views. Read-only: it never mutates the ledger.
    pub fn list_jobs(&self, limit: usize) -> Result<Vec<JobSummary>, JournalError> {
        let mut statement = self.connection.prepare(
            "SELECT j.id, j.started_unix_ms, j.completed_unix_ms, j.status, j.message,
                    j.request_json,
                    (SELECT COUNT(*) FROM job_items i WHERE i.job_id = j.id),
                    (SELECT COUNT(*) FROM job_items i
                      WHERE i.job_id = j.id AND i.status = 'applied'),
                    (SELECT COUNT(*) FROM job_items i
                      WHERE i.job_id = j.id AND i.status = 'failed'),
                    (SELECT COUNT(*) FROM job_items i
                      WHERE i.job_id = j.id AND i.status = 'rolled_back')
             FROM jobs j ORDER BY j.id DESC LIMIT ?1",
        )?;
        let jobs = statement
            .query_map(params![i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
                let request_json: String = row.get(5)?;
                let operation = serde_json::from_str::<crate::domain::PlanRequest>(&request_json)
                    .map(|request| request.operation)
                    .unwrap_or(crate::domain::OperationKind::Rename);
                let count = |value: i64| usize::try_from(value).unwrap_or(0);
                Ok(JobSummary {
                    id: row.get(0)?,
                    started_unix_ms: row.get(1)?,
                    completed_unix_ms: row.get(2)?,
                    status: row.get(3)?,
                    message: row.get(4)?,
                    operation,
                    total_items: count(row.get(6)?),
                    applied_items: count(row.get(7)?),
                    failed_items: count(row.get(8)?),
                    rolled_back_items: count(row.get(9)?),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(jobs)
    }

    pub fn operation_kind(
        &self,
        job_id: i64,
    ) -> Result<crate::domain::OperationKind, JournalError> {
        let request_json: String = self.connection.query_row(
            "SELECT request_json FROM jobs WHERE id = ?1",
            params![job_id],
            |row| row.get(0),
        )?;
        Ok(serde_json::from_str::<crate::domain::PlanRequest>(&request_json)?.operation)
    }

    pub fn mark_rollback_status(
        &self,
        job_id: i64,
        index: usize,
        status: &str,
        message: Option<&str>,
    ) -> Result<(), JournalError> {
        self.connection.execute(
            "UPDATE job_items SET status = ?1, error_message = ?2 WHERE job_id = ?3 AND item_index = ?4",
            params![status, message, job_id, i64::try_from(index).unwrap_or(i64::MAX)],
        )?;
        Ok(())
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn now_unix_ms() -> i64 {
    let nanos = OffsetDateTime::now_utc().unix_timestamp_nanos();
    i64::try_from(nanos / 1_000_000).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CollisionPolicy, FileFingerprint, FileSnapshot, OperationKind, PlanSummary,
    };
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn plan() -> PlannedJob {
        PlannedJob {
            request: crate::domain::PlanRequest {
                inputs: vec![PathBuf::from("source.txt")],
                operation: OperationKind::Rename,
                destination_root: None,
                recursive: false,
                include_directories: false,
                collision_policy: CollisionPolicy::Block,
                allow_legacy_long_paths: false,
                sort: crate::domain::SortOrder::Path,
            },
            pipeline_json: "{}".into(),
            global_issues: vec![],
            actions: vec![crate::domain::PlannedAction {
                index: 0,
                snapshot: FileSnapshot {
                    source: PathBuf::from("source.txt"),
                    original_name: "source.txt".into(),
                    is_directory: false,
                    fingerprint: FileFingerprint {
                        size_bytes: 2,
                        modified_unix_nanos: Some(10),
                    },
                    modified_unix_nanos: Some(10),
                    jpeg_exif_datetime: None,
                },
                destination: PathBuf::from("target.txt"),
                proposed_name: "target.txt".into(),
                stages: vec![],
                issues: vec![],
            }],
            summary: PlanSummary {
                discovered: 1,
                ready: 1,
                unchanged: 0,
                warnings: 0,
                blocking_errors: 0,
                cross_volume_candidates: 0,
            },
            sequence_gaps: vec![],
        }
    }

    #[test]
    fn journal_persists_applied_item() {
        let directory = tempdir().unwrap();
        let mut journal = Journal::open(directory.path().join("operations.sqlite")).unwrap();
        let job = journal.create_job(&plan()).unwrap();
        journal
            .mark_applied(
                job,
                0,
                &FileFingerprint {
                    size_bytes: 2,
                    modified_unix_nanos: Some(11),
                },
            )
            .unwrap();
        assert_eq!(journal.applied_items(job).unwrap().len(), 1);
    }
    #[test]
    fn list_jobs_summarizes_recorded_jobs() {
        let directory = tempdir().unwrap();
        let mut journal = Journal::open(directory.path().join("operations.sqlite")).unwrap();
        let job = journal.create_job(&plan()).unwrap();
        journal
            .mark_applied(
                job,
                0,
                &FileFingerprint {
                    size_bytes: 2,
                    modified_unix_nanos: Some(11),
                },
            )
            .unwrap();
        journal.finish_job(job, "completed", None).unwrap();
        let jobs = journal.list_jobs(10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, job);
        assert_eq!(jobs[0].status, "completed");
        assert_eq!(jobs[0].total_items, 1);
        assert_eq!(jobs[0].applied_items, 1);
        assert_eq!(jobs[0].failed_items, 0);
        assert!(matches!(jobs[0].operation, OperationKind::Rename));
        assert!(journal.list_jobs(0).unwrap().is_empty());
    }
}
