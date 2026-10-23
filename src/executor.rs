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

        if needs_staging {
            for action in &actions {
                if cancellation.is_cancelled() {
                    restore_staged(&self.journal, job_id, &staged);
                    self.journal.finish_job(
                        job_id,
                        "cancelled",
                        Some("Cancelled before applying staged items."),
                    )?;
                    return Ok(JobOutcome {
                        job_id,
                        applied: 0,
                        skipped: 0,
                        failed: 0,
                        cancelled: true,
                    });
                }
                if action.snapshot.is_directory {
                    self.journal.mark_failed(
                        job_id,
                        action.index,
                        "Directory move/rename execution is deferred in this build.",
                    )?;
                    restore_staged(&self.journal, job_id, &staged);
                    self.journal.finish_job(
                        job_id,
                        "failed",
                        Some("Directory operation is not supported."),
                    )?;
                    return Err(ExecuteError::UnsupportedDirectory(
                        action.snapshot.source.clone(),
                    ));
                }

                let temporary = unique_temp_path(&action.snapshot.source, job_id, action.index)?;
                // Persist the intended recovery path before changing the source.
                self.journal.mark_staged(job_id, action.index, &temporary)?;
                move_without_overwrite(&action.snapshot.source, &temporary)?;
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
                // Anything still staged was not yet materialized. Restore it;
                // previously applied items remain safely undoable from the ledger.
                let remaining = staged
                    .iter()
                    .filter(|(staged_action, _)| staged_action.index >= action.index)
                    .cloned()
                    .collect::<Vec<_>>();
                restore_staged(&self.journal, job_id, &remaining);
                self.journal.finish_job(
                    job_id,
                    "cancelled",
                    Some("Stopped safely between items; completed items can be undone."),
                )?;
                return Ok(outcome);
            }

            progress(ExecuteProgress {
                completed: position,
                total,
                current_path: action.snapshot.source.clone(),
                message: "Applying change".into(),
            });

            let source = match plan.request.operation {
                OperationKind::Copy => &action.snapshot.source,
                OperationKind::Rename | OperationKind::Move => staged
                    .iter()
                    .find(|(staged_action, _)| staged_action.index == action.index)
                    .map(|(_, temporary)| temporary)
                    .expect("every move/rename action was staged"),
            };

            let result = match plan.request.operation {
                OperationKind::Rename | OperationKind::Move => {
                    move_or_copy_to_target(source, &action.destination, true)
                }
                OperationKind::Copy => move_or_copy_to_target(source, &action.destination, false),
            };
            match result {
                Ok(()) => {
                    let fingerprint = fingerprint(&action.destination)?;
                    self.journal
                        .mark_applied(job_id, action.index, &fingerprint)?;
                    outcome.applied += 1;
                }
                Err(error) => {
                    let message = error.to_string();
                    self.journal.mark_failed(job_id, action.index, &message)?;
                    outcome.failed += 1;
                    let remaining = staged
                        .iter()
                        .filter(|(staged_action, _)| staged_action.index > action.index)
                        .cloned()
                        .collect::<Vec<_>>();
                    restore_staged(&self.journal, job_id, &remaining);
                    self.journal.finish_job(
                        job_id,
                        "failed",
                        Some(
                            "One or more changes failed. Applied items remain available for undo.",
                        ),
                    )?;
                    return Ok(outcome);
                }
            }
        }

        self.journal.finish_job(job_id, "completed", None)?;
        progress(ExecuteProgress {
            completed: total,
            total,
            current_path: PathBuf::new(),
            message: "Completed".into(),
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
        })
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
                reason: "The file changed after this job, so FilenameWeaver will not overwrite it."
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
        if let Err(error) = move_without_overwrite(&item.destination, &temp) {
            report.skipped.push(RollbackBlockedItem {
                index: item.index,
                destination: item.destination,
                reason: format!("Could not stage for safe rollback: {error}"),
            });
            continue;
        }
        journal.mark_rollback_status(job_id, item.index, "rollback_staged", None)?;
        staged.push((item, temp));
    }

    for (item, temp) in staged {
        match move_or_copy_to_target(&temp, &item.source, true) {
            Ok(()) => {
                journal.mark_rollback_status(job_id, item.index, "rolled_back", None)?;
                report.restored += 1;
            }
            Err(error) => {
                // The temporary file remains in place so that the user does not
                // lose content. The journal points to it through the original
                // staged record, and the report is explicit.
                report.skipped.push(RollbackBlockedItem {
                    index: item.index,
                    destination: item.destination,
                    reason: format!("Could not restore original location; temporary file retained at {}: {error}", temp.display()),
                });
            }
        }
    }
    Ok(())
}

fn restore_staged(journal: &Journal, job_id: i64, staged: &[(PlannedAction, PathBuf)]) {
    for (action, temp) in staged.iter().rev() {
        if temp.exists() && !action.snapshot.source.exists() {
            if let Err(error) = move_without_overwrite(temp, &action.snapshot.source) {
                let _ = journal.mark_failed(
                    job_id,
                    action.index,
                    &format!("Could not restore staging file {}: {error}", temp.display()),
                );
            } else {
                let _ = journal.mark_rollback_status(job_id, action.index, "not_applied", None);
            }
        }
    }
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
            ".{name}.filenameweaver-{job_id}-{index}-{attempt}.partial"
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
                    source: source_path,
                    is_directory: false,
                    fingerprint: FileFingerprint {
                        size_bytes: 1,
                        modified_unix_nanos: None,
                    },
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
