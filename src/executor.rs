use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use thiserror::Error;

use crate::domain::{FileFingerprint, JobOutcome, OperationKind, PlannedAction, PlannedJob};
use crate::journal::{AppliedItem, Journal, JournalError, RollbackBlockedItem, RollbackReport};

#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone)]
pub struct ExecuteProgress {
    pub completed: usize,
    pub total: usize,
    pub current_path: PathBuf,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum ExecuteError {
    #[error("the plan has blocking errors or no ready changes")]
    PlanNotExecutable,
    #[error("{cause}; recovery: {recovery:?}")]
    Recovery {
        cause: String,
        recovery: RecoveryResult,
    },
    #[error("operation journal error: {0}")]
    Journal(#[from] JournalError),
    #[error("filesystem error while handling {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the planner supplied an unsupported directory copy/move action: {0}")]
    UnsupportedDirectory(PathBuf),
    #[error("the staged file for {0} was missing when its turn came; the job was stopped safely")]
    StagingMismatch(PathBuf),
}

/// How one applied item compares with the state recorded when its job ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Divergence {
    /// The destination still has the recorded post-operation fingerprint.
    Intact,
    /// The destination exists but its size or modification time changed.
    Changed,
    /// The destination can no longer be read (moved away or deleted).
    Missing,
}

#[derive(Debug, Clone)]
pub struct ItemReview {
    pub item: AppliedItem,
    pub divergence: Divergence,
    /// For rename/move jobs: something else now sits at the original path,
    /// so a rollback of this item would be skipped rather than overwrite it.
    pub original_occupied: bool,
}

#[derive(Debug, Clone)]
pub struct JobReview {
    pub job_id: i64,
    pub operation: OperationKind,
    pub items: Vec<ItemReview>,
    pub pending: Vec<crate::journal::RecoverableItem>,
}

impl JobReview {
    pub fn count(&self, divergence: Divergence) -> usize {
        self.items
            .iter()
            .filter(|review| review.divergence == divergence)
            .count()
    }

    /// Items a rollback could act on right now: unchanged files whose
    /// original location (for rename/move) is still free.
    pub fn safely_reversible(&self) -> usize {
        self.items
            .iter()
            .filter(|review| review.divergence == Divergence::Intact && !review.original_occupied)
            .count()
            + self
                .pending
                .iter()
                .filter(|item| {
                    !item.source.exists()
                        && fingerprint(pending_recovery_path(item)).ok().as_ref()
                            == Some(&item.source_fingerprint)
                })
                .count()
    }
}

pub struct Executor {
    journal: Journal,
}

impl Executor {
    pub fn new(journal: Journal) -> Self {
        Self { journal }
    }

    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    /// Applies a validated job. The plan is journaled before any source is
    /// changed. Rename and move operations first stage their sources using
    /// unique temporary names, making swap/cycle and case-only operations safe.
    pub fn execute<F>(
        &mut self,
        plan: &PlannedJob,
        cancellation: &CancellationToken,
        mut progress: F,
    ) -> Result<JobOutcome, ExecuteError>
    where
        F: FnMut(ExecuteProgress),
    {
        if !plan.is_executable() {
            return Err(ExecuteError::PlanNotExecutable);
        }
        let job_id = self.journal.create_job(plan)?;
        let actions = plan
            .actions
            .iter()
            .filter(|action| action.will_change() && !action.is_blocked())
            .cloned()
            .collect::<Vec<_>>();
        let total = actions.len();
        let mut staged = Vec::new();
        let needs_staging = matches!(
            plan.request.operation,
            OperationKind::Rename | OperationKind::Move
        );

        let result = (|| -> Result<JobOutcome, ExecuteError> {
            if needs_staging {
                for action in &actions {
                    if cancellation.is_cancelled() {
                        break;
                    }
                    if action.snapshot.is_directory {
                        return Err(ExecuteError::UnsupportedDirectory(
                            action.snapshot.source.clone(),
                        ));
                    }
                    let temporary =
                        unique_temp_path(&action.snapshot.source, job_id, action.index)?;
                    self.journal.mark_staged(job_id, action.index, &temporary)?;
                    if let Err(error) = move_without_overwrite(&action.snapshot.source, &temporary)
                    {
                        let persistence =
                            self.journal
                                .mark_failed(job_id, action.index, &error.to_string());
                        return Err(ExecuteError::Recovery {
                            cause: format!(
                                "{error}; staging intent at {}; journal update: {persistence:?}",
                                temporary.display()
                            ),
                            recovery: RecoveryResult::default(),
                        });
                    }
                    staged.push((action.clone(), temporary));
                }
            }
            let mut outcome = JobOutcome {
                job_id,
                applied: 0,
                skipped: 0,
                failed: 0,
                cancelled: false,
            };
            for (position, action) in actions.iter().enumerate() {
                if cancellation.is_cancelled() {
                    outcome.cancelled = true;
                    break;
                }
                progress(ExecuteProgress {
                    completed: position,
                    total,
                    current_path: action.snapshot.source.clone(),
                    message: "Applying change".into(),
                });
                let source = if needs_staging {
                    staged
                        .iter()
                        .find(|(item, _)| item.index == action.index)
                        .map(|(_, path)| path.as_path())
                        .ok_or_else(|| {
                            ExecuteError::StagingMismatch(action.snapshot.source.clone())
                        })?
                } else {
                    &action.snapshot.source
                };
                if let Err(error) =
                    move_or_copy_to_target(source, &action.destination, needs_staging)
                {
                    self.journal
                        .mark_failed(job_id, action.index, &error.to_string())?;
                    outcome.failed += 1;
                    break;
                }
                // Materialized items must never be restored from their now absent stage.
                staged.retain(|(item, _)| item.index != action.index);
                let recorded = fingerprint(&action.destination).and_then(|fingerprint| {
                    self.journal
                        .mark_applied(job_id, action.index, &fingerprint)?;
                    Ok(())
                });
                if let Err(error) = recorded {
                    // The pre-mutation staging intent remains durable even if this write fails.
                    let persistence = self.journal.mark_rollback_status(
                        job_id,
                        action.index,
                        "materialized",
                        Some(&error.to_string()),
                    );
                    return Err(ExecuteError::Recovery {
                        cause: format!(
                            "{error}; materialized file at {}; journal update: {persistence:?}",
                            action.destination.display()
                        ),
                        recovery: RecoveryResult {
                            retained: vec![(action.destination.clone(), error.to_string())],
                            ..RecoveryResult::default()
                        },
                    });
                }
                outcome.applied += 1;
            }
            Ok(outcome)
        })();
        // This runs for every fallible exit after staging, independently of journal health.
        let mut recovery = restore_staged(&self.journal, job_id, &staged);
        let result = match result {
            Err(ExecuteError::Recovery {
                cause,
                recovery: materialized,
            }) => {
                recovery.retained.extend(materialized.retained);
                recovery
                    .filesystem_errors
                    .extend(materialized.filesystem_errors);
                recovery.journal_errors.extend(materialized.journal_errors);
                Err(ExecuteError::Recovery {
                    cause,
                    recovery: RecoveryResult::default(),
                })
            }
            other => other,
        };
        let outcome = match result {
            Ok(outcome) if recovery.retained.is_empty() && recovery.journal_errors.is_empty() => {
                outcome
            }
            other => {
                let cause = match other {
                    Ok(_) => "staging recovery incomplete".into(),
                    Err(error) => error.to_string(),
                };
                if let Err(error) = self.journal.finish_job(job_id, "failed", Some(&cause)) {
                    recovery.journal_errors.push(error.to_string());
                }
                return Err(ExecuteError::Recovery { cause, recovery });
            }
        };
        let status = if outcome.cancelled {
            "cancelled"
        } else if outcome.failed > 0 {
            "failed"
        } else {
            "completed"
        };
        self.journal.finish_job(job_id, status, None)?;
        progress(ExecuteProgress {
            completed: total,
            total,
            current_path: PathBuf::new(),
            message: status.into(),
        });
        Ok(outcome)
    }

    /// Reverses a journaled job only where the recorded destination still has
    /// the recorded post-operation fingerprint. This deliberately favors a
    /// skipped, explained item over overwriting a user’s later work.
    pub fn rollback(&mut self, job_id: i64) -> Result<RollbackReport, ExecuteError> {
        let kind = self.journal.operation_kind(job_id)?;
        let applied = self.journal.applied_items(job_id)?;
        let (safe, mut report) = preflight_rollback(applied);

        match kind {
            OperationKind::Copy => {
                for item in safe {
                    match fs::remove_file(&item.destination) {
                        Ok(()) => {
                            self.journal.mark_rollback_status(
                                job_id,
                                item.index,
                                "rolled_back",
                                None,
                            )?;
                            report.removed_copies += 1;
                        }
                        Err(error) => report.skipped.push(RollbackBlockedItem {
                            index: item.index,
                            destination: item.destination,
                            reason: format!("Could not remove the copied file: {error}"),
                        }),
                    }
                }
            }
            OperationKind::Rename | OperationKind::Move => {
                rollback_moves(&mut self.journal, job_id, safe, &mut report)?;
            }
        }
        let pending = self.journal.recoverable_items(job_id)?;
        for item in pending {
            let path = pending_recovery_path(&item);
            if fingerprint(path).ok().as_ref() != Some(&item.source_fingerprint) {
                report.skipped.push(RollbackBlockedItem {
                    index: item.index, destination: path.to_path_buf(),
                    reason: format!("Recovery content is missing or differs from the recorded source; inspect {} and {} before manual recovery (status {}).", item.temp.display(), item.destination.display(), item.status),
                });
                continue;
            }
            let result = recover_item(&self.journal, job_id, item.index, &item.source, path);
            report.restored += result.restored.len();
            for (path, reason) in result.retained {
                report.skipped.push(RollbackBlockedItem {
                    index: item.index,
                    destination: path,
                    reason,
                });
            }
            for reason in result.journal_errors {
                report.skipped.push(RollbackBlockedItem {
                    index: item.index,
                    destination: item.temp.clone(),
                    reason,
                });
            }
        }
        Ok(report)
    }

    /// Compares every applied item of a job with the current disk state,
    /// using the same conservative fingerprint check rollback performs.
    /// Read-only: nothing on disk or in the journal changes.
    pub fn review(&self, job_id: i64) -> Result<JobReview, ExecuteError> {
        let operation = self.journal.operation_kind(job_id)?;
        let applied = self.journal.applied_items(job_id)?;
        let items = applied
            .into_iter()
            .map(|item| {
                let divergence = match fingerprint(&item.destination) {
                    Ok(actual) if actual == item.result_fingerprint => Divergence::Intact,
                    Ok(_) => Divergence::Changed,
                    Err(_) => Divergence::Missing,
                };
                let original_occupied =
                    matches!(operation, OperationKind::Rename | OperationKind::Move)
                        && item.source.exists()
                        && path_key(&item.source) != path_key(&item.destination);
                ItemReview {
                    item,
                    divergence,
                    original_occupied,
                }
            })
            .collect();
        Ok(JobReview {
            job_id,
            operation,
            items,
            pending: self.journal.recoverable_items(job_id)?,
        })
    }
}

fn pending_recovery_path(item: &crate::journal::RecoverableItem) -> &Path {
    if !item.temp.exists() && matches!(item.status.as_str(), "materialized" | "staged") {
        &item.destination
    } else {
        &item.temp
    }
}

fn preflight_rollback(items: Vec<AppliedItem>) -> (Vec<AppliedItem>, RollbackReport) {
    let mut safe = Vec::new();
    let mut report = RollbackReport::default();
    for item in items {
        match fingerprint(&item.destination) {
            Ok(actual) if actual == item.result_fingerprint => safe.push(item),
            Ok(_) => report.skipped.push(RollbackBlockedItem {
                index: item.index,
                destination: item.destination,
                reason: "The file changed after this job, so FileWeaver will not overwrite it."
                    .into(),
            }),
            Err(error) => report.skipped.push(RollbackBlockedItem {
                index: item.index,
                destination: item.destination,
                reason: format!("The renamed file is unavailable: {error}"),
            }),
        }
    }
    (safe, report)
}

fn rollback_moves(
    journal: &mut Journal,
    job_id: i64,
    candidates: Vec<AppliedItem>,
    report: &mut RollbackReport,
) -> Result<(), ExecuteError> {
    let target_keys = candidates
        .iter()
        .map(|item| path_key(&item.destination))
        .collect::<std::collections::BTreeSet<_>>();
    let mut safe = Vec::new();
    for item in candidates {
        if item.source.exists() && !target_keys.contains(&path_key(&item.source)) {
            report.skipped.push(RollbackBlockedItem {
                index: item.index,
                destination: item.destination,
                reason: "The original location is occupied by another file, so it will not be overwritten.".into(),
            });
        } else {
            safe.push(item);
        }
    }

    let mut staged = Vec::new();
    for item in safe {
        let temp = match unique_temp_path(&item.destination, job_id, item.index) {
            Ok(path) => path,
            Err(error) => {
                report.skipped.push(RollbackBlockedItem {
                    index: item.index,
                    destination: item.destination,
                    reason: error.to_string(),
                });
                continue;
            }
        };
        if let Err(error) = journal.mark_staged(job_id, item.index, &temp) {
            report.skipped.push(RollbackBlockedItem {
                index: item.index,
                destination: item.destination,
                reason: format!("Could not persist rollback staging intent: {error}"),
            });
            continue;
        }
        if let Err(error) = move_without_overwrite(&item.destination, &temp) {
            report.skipped.push(RollbackBlockedItem {
                index: item.index,
                destination: item.destination,
                reason: format!("Could not stage for safe rollback: {error}"),
            });
            if let Err(error) = journal.mark_rollback_status(job_id, item.index, "applied", None) {
                report.skipped.push(RollbackBlockedItem {
                    index: item.index,
                    destination: temp,
                    reason: error.to_string(),
                });
            }
            continue;
        }
        // The intent already records the actual rollback stage path.
        staged.push((item, temp));
    }

    for (item, temp) in staged {
        match move_or_copy_to_target(&temp, &item.source, true) {
            Ok(()) => {
                if let Err(error) =
                    journal.mark_rollback_status(job_id, item.index, "rolled_back", None)
                {
                    report.skipped.push(RollbackBlockedItem {
                        index: item.index,
                        destination: item.source.clone(),
                        reason: format!("Original restored, but ledger update failed: {error}"),
                    });
                }
                report.restored += 1;
            }
            Err(error) => {
                let reason = format!(
                    "Could not restore original {}; temporary file retained at {}: {error}",
                    item.source.display(),
                    temp.display()
                );
                if let Err(error) = journal.mark_rollback_status(
                    job_id,
                    item.index,
                    "recovery_pending",
                    Some(&reason),
                ) {
                    report.skipped.push(RollbackBlockedItem {
                        index: item.index,
                        destination: temp.clone(),
                        reason: format!("Recovery status could not be persisted: {error}"),
                    });
                }
                report.skipped.push(RollbackBlockedItem {
                    index: item.index,
                    destination: temp.clone(),
                    reason: format!("Could not restore original location; temporary file retained at {}: {error}", temp.display()),
                });
            }
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct RecoveryResult {
    pub restored: Vec<PathBuf>,
    pub retained: Vec<(PathBuf, String)>,
    pub filesystem_errors: Vec<String>,
    pub journal_errors: Vec<String>,
}

fn recover_item(
    journal: &Journal,
    job_id: i64,
    index: usize,
    source: &Path,
    temp: &Path,
) -> RecoveryResult {
    let mut result = RecoveryResult::default();
    let (status, reason) = match move_without_overwrite(temp, source) {
        Ok(()) => {
            result.restored.push(source.to_path_buf());
            ("not_applied", None)
        }
        Err(error) => {
            let reason = format!(
                "Recovery file retained at {} for original {}: {error}",
                temp.display(),
                source.display()
            );
            result.filesystem_errors.push(error.to_string());
            result.retained.push((temp.to_path_buf(), reason.clone()));
            ("recovery_pending", Some(reason))
        }
    };
    if let Err(error) = journal.mark_rollback_status(job_id, index, status, reason.as_deref()) {
        result.journal_errors.push(format!(
            "{} -> {}: {error}",
            temp.display(),
            source.display()
        ));
    }
    result
}

fn restore_staged(
    journal: &Journal,
    job_id: i64,
    staged: &[(PlannedAction, PathBuf)],
) -> RecoveryResult {
    let mut result = RecoveryResult::default();
    for (action, temp) in staged.iter().rev() {
        let item = recover_item(journal, job_id, action.index, &action.snapshot.source, temp);
        result.restored.extend(item.restored);
        result.retained.extend(item.retained);
        result.filesystem_errors.extend(item.filesystem_errors);
        result.journal_errors.extend(item.journal_errors);
    }
    result
}

fn move_or_copy_to_target(
    source: &Path,
    target: &Path,
    remove_source: bool,
) -> Result<(), ExecuteError> {
    if target.exists() {
        return Err(ExecuteError::Io {
            path: target.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "target appeared after preview",
            ),
        });
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|source| ExecuteError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    if remove_source {
        match fs::rename(source, target) {
            Ok(()) => return Ok(()),
            Err(error) if is_cross_device(&error) => {}
            Err(rename_error) => {
                return Err(ExecuteError::Io {
                    path: source_path_for_error(source, target),
                    source: rename_error,
                });
            }
        }
    }

    copy_to_unique_temp_then_commit(source, target)?;
    if remove_source {
        fs::remove_file(source).map_err(|source_error| ExecuteError::Io {
            path: source.to_path_buf(),
            source: source_error,
        })?;
    }
    Ok(())
}

fn copy_to_unique_temp_then_commit(source: &Path, target: &Path) -> Result<(), ExecuteError> {
    let temp = unique_temp_path(target, 0, 0)?;
    let copy_result = fs::copy(source, &temp).map_err(|source_error| ExecuteError::Io {
        path: source.to_path_buf(),
        source: source_error,
    });
    if let Err(error) = copy_result {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    // Flush the copied bytes to disk before the size check and the
    // non-overwriting commit rename (Contract §3 #14), so a crash cannot
    // leave a committed destination whose contents never reached the disk.
    let flush_result = fs::OpenOptions::new()
        .write(true)
        .open(&temp)
        .and_then(|file| file.sync_all());
    if let Err(error) = flush_result {
        let _ = fs::remove_file(&temp);
        return Err(ExecuteError::Io {
            path: temp,
            source: error,
        });
    }
    // A size comparison catches short writes; it is not a content-integrity
    // guarantee. Cryptographic verification is deliberately a later option.
    let source_size = fs::metadata(source)
        .map_err(|source_error| ExecuteError::Io {
            path: source.to_path_buf(),
            source: source_error,
        })?
        .len();
    let target_size = fs::metadata(&temp)
        .map_err(|source_error| ExecuteError::Io {
            path: temp.clone(),
            source: source_error,
        })?
        .len();
    if source_size != target_size {
        let _ = fs::remove_file(&temp);
        return Err(ExecuteError::Io {
            path: target.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::WriteZero,
                "copied byte count does not match source",
            ),
        });
    }
    move_without_overwrite(&temp, target)
}

fn unique_temp_path(path: &Path, job_id: i64, index: usize) -> Result<PathBuf, ExecuteError> {
    let parent = path.parent().ok_or_else(|| ExecuteError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory"),
    })?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    for attempt in 0..1_000_u32 {
        let candidate = parent.join(format!(
            ".{name}.fileweaver-{job_id}-{index}-{attempt}.partial"
        ));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(ExecuteError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a unique staging filename",
        ),
    })
}

fn move_without_overwrite(source: &Path, target: &Path) -> Result<(), ExecuteError> {
    if target.exists() {
        return Err(ExecuteError::Io {
            path: target.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "refusing to overwrite an existing file",
            ),
        });
    }
    fs::rename(source, target).map_err(|source_error| ExecuteError::Io {
        path: source_path_for_error(source, target),
        source: source_error,
    })
}

fn fingerprint(path: &Path) -> Result<FileFingerprint, ExecuteError> {
    fs::metadata(path)
        .map(|metadata| FileFingerprint::from_metadata(&metadata))
        .map_err(|source| ExecuteError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn is_cross_device(error: &io::Error) -> bool {
    // ERROR_NOT_SAME_DEVICE is 17 on Windows; EXDEV is commonly 18 on Unix.
    matches!(error.raw_os_error(), Some(17 | 18))
}

fn source_path_for_error(source: &Path, target: &Path) -> PathBuf {
    if source.exists() {
        source.to_path_buf()
    } else {
        target.to_path_buf()
    }
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CollisionPolicy, FileFingerprint, FileSnapshot, OperationKind, PlanRequest, PlanSummary,
        PlannedAction, PlannedJob, SortOrder,
    };
    use crate::journal::Journal;
    use tempfile::tempdir;

    fn manual_swap_plan(directory: &Path) -> PlannedJob {
        let action = |index: usize, source: &str, destination: &str| {
            let source_path = directory.join(source);
            PlannedAction {
                index,
                snapshot: FileSnapshot {
                    original_name: source.into(),
                    source: source_path.clone(),
                    is_directory: false,
                    fingerprint: fs::metadata(&source_path)
                        .map(|metadata| FileFingerprint::from_metadata(&metadata))
                        .unwrap_or(FileFingerprint {
                            size_bytes: 1,
                            modified_unix_nanos: None,
                        }),
                    modified_unix_nanos: None,
                    jpeg_exif_datetime: None,
                },
                destination: directory.join(destination),
                proposed_name: destination.into(),
                stages: vec![],
                issues: vec![],
            }
        };
        PlannedJob {
            request: PlanRequest {
                inputs: vec![directory.to_path_buf()],
                operation: OperationKind::Rename,
                destination_root: None,
                recursive: false,
                include_directories: false,
                collision_policy: CollisionPolicy::Block,
                allow_legacy_long_paths: false,
                sort: SortOrder::Path,
            },
            pipeline_json: "{}".into(),
            global_issues: vec![],
            actions: vec![action(0, "a.txt", "b.txt"), action(1, "b.txt", "a.txt")],
            summary: PlanSummary {
                discovered: 2,
                ready: 2,
                unchanged: 0,
                warnings: 0,
                blocking_errors: 0,
                cross_volume_candidates: 0,
            },
            sequence_gaps: vec![],
        }
    }

    #[test]
    fn second_stage_failure_restores_first_and_reopens_history() {
        // Test fixture operations are expected to succeed.
        let directory = tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a.txt"), "A").unwrap();
        let database = root.join("operations.sqlite");
        let mut executor = Executor::new(Journal::open(&database).unwrap());
        assert!(
            executor
                .execute(
                    &manual_swap_plan(root),
                    &CancellationToken::default(),
                    |_| {}
                )
                .is_err()
        );
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"A");
        drop(executor);
        let journal = Journal::open(database).unwrap();
        let jobs = journal.list_jobs(10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(
            journal
                .recoverable_items(jobs[0].id)
                .unwrap()
                .iter()
                .all(|item| item.source != root.join("a.txt"))
        );
    }

    #[test]
    fn appeared_destination_restores_current_and_remaining_stages() {
        // Test fixture operations are expected to succeed.
        let directory = tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a.txt"), "A").unwrap();
        fs::write(root.join("b.txt"), "B").unwrap();
        let mut plan = manual_swap_plan(root);
        plan.actions[0].destination = root.join("occupied.txt");
        fs::write(root.join("occupied.txt"), "sentinel").unwrap();
        let database = root.join("operations.sqlite");
        let mut executor = Executor::new(Journal::open(&database).unwrap());
        let outcome = executor
            .execute(&plan, &CancellationToken::default(), |_| {})
            .unwrap();
        assert_eq!(outcome.failed, 1);
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"A");
        assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"B");
        assert_eq!(fs::read(root.join("occupied.txt")).unwrap(), b"sentinel");
        drop(executor);
        assert!(
            Journal::open(database)
                .unwrap()
                .recoverable_items(outcome.job_id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn occupied_original_retains_stage_and_retry_preserves_sentinel() {
        // Test fixture operations are expected to succeed.
        let directory = tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a.txt"), "A").unwrap();
        fs::write(root.join("b.txt"), "B").unwrap();
        let database = root.join("operations.sqlite");
        let mut executor = Executor::new(Journal::open(&database).unwrap());
        let error = executor
            .execute(
                &manual_swap_plan(root),
                &CancellationToken::default(),
                |_| {
                    fs::write(root.join("a.txt"), "sentinel").unwrap();
                    fs::write(root.join("b.txt"), "block destination").unwrap();
                },
            )
            .unwrap_err();
        assert!(matches!(error, ExecuteError::Recovery { .. }));
        drop(executor);
        let mut executor = Executor::new(Journal::open(database).unwrap());
        let job = executor.journal().list_jobs(1).unwrap()[0].id;
        assert_eq!(executor.review(job).unwrap().pending.len(), 2);
        assert_eq!(executor.rollback(job).unwrap().restored, 0);
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"sentinel");
        fs::remove_file(root.join("a.txt")).unwrap();
        fs::remove_file(root.join("b.txt")).unwrap();
        assert_eq!(executor.rollback(job).unwrap().restored, 2);
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"A");
        assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"B");
    }

    #[test]
    fn cancellation_recovers_all_outstanding_stages() {
        // Test fixture operations are expected to succeed.
        let directory = tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a.txt"), "A").unwrap();
        fs::write(root.join("b.txt"), "B").unwrap();
        let mut plan = manual_swap_plan(root);
        plan.actions[0].destination = root.join("c.txt");
        let cancellation = CancellationToken::default();
        let mut executor = Executor::new(Journal::open(root.join("operations.sqlite")).unwrap());
        let outcome = executor
            .execute(&plan, &cancellation, |_| cancellation.cancel())
            .unwrap();
        assert!(outcome.cancelled);
        assert_eq!(outcome.applied, 1);
        assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"B");
        assert!(
            executor
                .journal()
                .recoverable_items(outcome.job_id)
                .unwrap()
                .is_empty()
        );
        assert_eq!(executor.rollback(outcome.job_id).unwrap().restored, 1);
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"A");
    }

    #[test]
    fn journal_failure_does_not_prevent_stage_restoration() {
        // Test fixture operations are expected to succeed.
        let directory = tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a.txt"), "A").unwrap();
        fs::write(root.join("b.txt"), "B").unwrap();
        let database = root.join("operations.sqlite");
        let mut plan = manual_swap_plan(root);
        plan.actions[0].destination = root.join("c.txt");
        let mut executor = Executor::new(Journal::open(&database).unwrap());
        let error = executor.execute(&plan, &CancellationToken::default(), |_| {
            rusqlite::Connection::open(&database).unwrap().execute_batch(
                "CREATE TRIGGER fail_updates BEFORE UPDATE ON job_items BEGIN SELECT RAISE(FAIL, 'injected ledger failure'); END;"
            ).unwrap();
        }).unwrap_err();
        assert!(error.to_string().contains("c.txt"));
        assert_eq!(fs::read(root.join("c.txt")).unwrap(), b"A");
        assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"B");
        drop(executor);
        let journal = Journal::open(database).unwrap();
        let job = journal.list_jobs(1).unwrap()[0].id;
        assert!(
            journal
                .recoverable_items(job)
                .unwrap()
                .iter()
                .any(|item| item.destination == root.join("c.txt"))
        );
        drop(journal);
        rusqlite::Connection::open(&root.join("operations.sqlite"))
            .unwrap()
            .execute_batch("DROP TRIGGER fail_updates;")
            .unwrap();
        let mut executor = Executor::new(Journal::open(root.join("operations.sqlite")).unwrap());
        assert_eq!(executor.rollback(job).unwrap().restored, 1);
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"A");
        assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"B");
    }

    #[test]
    fn second_stage_journal_failure_still_restores_first() {
        // Test fixture operations are expected to succeed.
        let directory = tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a.txt"), "A").unwrap();
        fs::write(root.join("b.txt"), "B").unwrap();
        let database = root.join("operations.sqlite");
        let mut executor = Executor::new(Journal::open(&database).unwrap());
        rusqlite::Connection::open(&database).unwrap().execute_batch(
            "CREATE TRIGGER fail_second_stage BEFORE UPDATE ON job_items WHEN NEW.item_index = 1 AND NEW.status = 'staged' BEGIN SELECT RAISE(FAIL, 'injected stage journal failure'); END;"
        ).unwrap();
        assert!(
            executor
                .execute(
                    &manual_swap_plan(root),
                    &CancellationToken::default(),
                    |_| {}
                )
                .is_err()
        );
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"A");
        assert_eq!(fs::read(root.join("b.txt")).unwrap(), b"B");
        drop(executor);
        let journal = Journal::open(database).unwrap();
        let job = journal.list_jobs(1).unwrap()[0].id;
        assert!(journal.recoverable_items(job).unwrap().is_empty());
    }

    #[test]
    fn failed_restore_retains_explicit_paths_after_reopening() {
        // Test fixture operations are expected to succeed.
        let directory = tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a.txt"), "A").unwrap();
        fs::write(root.join("b.txt"), "B").unwrap();
        let database = root.join("operations.sqlite");
        let plan = manual_swap_plan(root);
        let mut journal = Journal::open(&database).unwrap();
        let job = journal.create_job(&plan).unwrap();
        let temp = root.join("retained.partial");
        journal.mark_staged(job, 0, &temp).unwrap();
        fs::rename(root.join("a.txt"), &temp).unwrap();
        // A missing original parent forces a filesystem restoration failure.
        let source = root.join("missing-parent/a.txt");
        let result = recover_item(&journal, job, 0, &source, &temp);
        assert_eq!(result.filesystem_errors.len(), 1);
        assert_eq!(result.retained[0].0, temp);
        drop(journal);
        let journal = Journal::open(database).unwrap();
        let pending = journal.recoverable_items(job).unwrap();
        assert_eq!(pending[0].temp, temp);
        assert!(
            pending[0]
                .reason
                .as_ref()
                .unwrap()
                .contains("missing-parent")
        );
        assert_eq!(fs::read(temp).unwrap(), b"A");
    }

    #[test]
    fn cycle_rename_is_staged_safely_and_can_be_rolled_back() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "A").unwrap();
        fs::write(directory.path().join("b.txt"), "B").unwrap();
        let journal = Journal::open(directory.path().join("operations.sqlite")).unwrap();
        let mut executor = Executor::new(journal);
        let outcome = executor
            .execute(
                &manual_swap_plan(directory.path()),
                &CancellationToken::default(),
                |_| {},
            )
            .unwrap();
        assert_eq!(outcome.applied, 2);
        assert_eq!(
            fs::read_to_string(directory.path().join("a.txt")).unwrap(),
            "B"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("b.txt")).unwrap(),
            "A"
        );
        let rollback = executor.rollback(outcome.job_id).unwrap();
        assert_eq!(rollback.restored, 2);
        assert_eq!(
            fs::read_to_string(directory.path().join("a.txt")).unwrap(),
            "A"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("b.txt")).unwrap(),
            "B"
        );
    }
    #[test]
    fn review_reports_intact_changed_and_missing_items() {
        use crate::rules::{Rule, RulePipeline};
        let directory = tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("a.txt"), b"a").unwrap();
        fs::write(root.join("b.txt"), b"b").unwrap();
        fs::write(root.join("c.txt"), b"c").unwrap();
        let request = PlanRequest {
            inputs: vec![root.to_path_buf()],
            operation: OperationKind::Rename,
            destination_root: None,
            recursive: false,
            include_directories: false,
            collision_policy: CollisionPolicy::Block,
            allow_legacy_long_paths: false,
            sort: SortOrder::Path,
        };
        let pipeline = RulePipeline {
            rules: vec![Rule::AddPrefix { text: "x-".into() }],
        };
        let planned = crate::planner::plan(request, &pipeline).unwrap();
        let journal = Journal::open(root.join("operations.sqlite")).unwrap();
        let mut executor = Executor::new(journal);
        let outcome = executor
            .execute(&planned, &CancellationToken::default(), |_| {})
            .unwrap();

        let review = executor.review(outcome.job_id).unwrap();
        assert_eq!(review.count(Divergence::Intact), 3);
        assert_eq!(review.safely_reversible(), 3);

        fs::write(root.join("x-a.txt"), b"a-much-longer-content").unwrap();
        fs::remove_file(root.join("x-b.txt")).unwrap();
        fs::write(root.join("c.txt"), b"intruder").unwrap();

        let review = executor.review(outcome.job_id).unwrap();
        assert_eq!(review.count(Divergence::Changed), 1);
        assert_eq!(review.count(Divergence::Missing), 1);
        assert_eq!(review.count(Divergence::Intact), 1);
        // The one intact file's original location is occupied again.
        assert_eq!(review.safely_reversible(), 0);
    }
}
