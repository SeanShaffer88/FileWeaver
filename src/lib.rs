//! Core services for FileWeaver.
//!
//! The crate deliberately separates planning from filesystem mutation. A plan
//! can be inspected and tested without changing a file. Execution accepts
//! only an already validated plan and writes a durable local journal, which
//! can later be reviewed against the current disk state and rolled back.

pub mod domain;
pub mod executor;
pub mod handoff;
pub mod journal;
pub mod metadata;
pub mod planner;
pub mod rules;

pub use domain::{
    CollisionPolicy, FileFingerprint, FileSnapshot, JobOutcome, OperationKind, PlanIssue,
    PlanIssueKind, PlanRequest, PlannedAction, PlannedJob, SequenceGap, Severity, SortOrder,
};
pub use executor::{
    CancellationToken, Divergence, ExecuteError, ExecuteProgress, Executor, ItemReview, JobReview,
};
pub use handoff::{DriftReport, DriftState, HandoffAction, HandoffError, PlanFile, RowDrift};
pub use journal::{JobSummary, Journal, JournalError, RollbackReport};
pub use planner::{PlanError, plan, subset_plan};
pub use rules::{CaseStyle, DateSource, Rule, RulePipeline, RuleProblem};
