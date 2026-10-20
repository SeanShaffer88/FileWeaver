use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// A lightweight, conservative fingerprint used to reject an unsafe rollback.
///
/// It is intentionally *not* a cryptographic identity. A future Windows-only
/// identity provider can strengthen it with volume/file IDs without changing
/// journal records. The executor records a post-operation fingerprint before an
/// item becomes undoable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub size_bytes: u64,
    pub modified_unix_nanos: Option<i128>,
}

impl FileFingerprint {
    pub fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        let modified_unix_nanos = metadata.modified().ok().and_then(system_time_to_nanos);
        Self {
            size_bytes: metadata.len(),
            modified_unix_nanos,
        }
    }
}

fn system_time_to_nanos(time: SystemTime) -> Option<i128> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => Some(i128::try_from(duration.as_nanos()).ok()?),
        Err(error) => {
            let duration = error.duration();
            Some(-i128::try_from(duration.as_nanos()).ok()?)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSnapshot {
    pub source: PathBuf,
    pub original_name: String,
    pub is_directory: bool,
    pub fingerprint: FileFingerprint,
    /// Local filesystem time, if the platform supplied one. We never assume it
    /// is capture time; photo capture time is a separate, optional field.
    pub modified_unix_nanos: Option<i128>,
    pub jpeg_exif_datetime: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationKind {
    Rename,
    Copy,
    Move,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CollisionPolicy {
    /// A target collision is a blocking error. This is the only initial policy.
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanRequest {
    pub inputs: Vec<PathBuf>,
    pub operation: OperationKind,
    /// Required for copy/move. A caller may also use it with rename, in which
    /// case the rename becomes a reorganization into that directory.
    pub destination_root: Option<PathBuf>,
    pub recursive: bool,
    pub include_directories: bool,
    pub collision_policy: CollisionPolicy,
    pub allow_legacy_long_paths: bool,
}

impl PlanRequest {
    pub fn rename(inputs: Vec<PathBuf>) -> Self {
        Self {
            inputs,
            operation: OperationKind::Rename,
            destination_root: None,
            recursive: false,
            include_directories: false,
            collision_policy: CollisionPolicy::Block,
            allow_legacy_long_paths: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanIssueKind {
    InputUnavailable,
    PermissionDenied,
    UnsupportedDirectory,
    EmptyName,
    InvalidCharacter,
    ReservedWindowsName,
    TrailingDotOrSpace,
    ComponentTooLong,
    LegacyPathTooLong,
    DuplicateTarget,
    ExistingTarget,
    MissingDestinationRoot,
    InvalidRule,
    MissingMetadata,
    NoChange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanIssue {
    pub severity: Severity,
    pub kind: PlanIssueKind,
    pub message: String,
}

impl PlanIssue {
    pub fn error(kind: PlanIssueKind, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            kind,
            message: message.into(),
        }
    }

    pub fn warning(kind: PlanIssueKind, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            kind,
            message: message.into(),
        }
    }

    pub fn info(kind: PlanIssueKind, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Info,
            kind,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageValue {
    pub rule_label: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedAction {
    pub index: usize,
    pub snapshot: FileSnapshot,
    pub destination: PathBuf,
    pub proposed_name: String,
    pub stages: Vec<StageValue>,
    pub issues: Vec<PlanIssue>,
}

impl PlannedAction {
    pub fn is_blocked(&self) -> bool {
        self.issues
            .iter()
            .any(|issue| issue.severity == Severity::Error)
    }

    pub fn will_change(&self) -> bool {
        self.snapshot.source != self.destination
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSummary {
    pub discovered: usize,
    pub ready: usize,
    pub unchanged: usize,
    pub warnings: usize,
    pub blocking_errors: usize,
    pub cross_volume_candidates: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedJob {
    pub request: PlanRequest,
    pub pipeline_json: String,
    /// Problems not associated with a single source, for example a missing
    /// destination root. The same summary counters include these issues.
    pub global_issues: Vec<PlanIssue>,
    pub actions: Vec<PlannedAction>,
    pub summary: PlanSummary,
}

impl PlannedJob {
    pub fn is_executable(&self) -> bool {
        self.summary.blocking_errors == 0 && self.summary.ready > 0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobOutcome {
    pub job_id: i64,
    pub applied: usize,
    pub skipped: usize,
    pub failed: usize,
    pub cancelled: bool,
}
