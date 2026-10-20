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

/// The order in which discovered files are numbered and processed. The order
/// is part of the plan: it is shown to the user before applying and recorded
/// in the journal with the job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortOrder {
    /// Case-insensitive full path. The original FileWeaver order.
    #[default]
    Path,
    /// Case-insensitive file name; ties fall back to full path.
    Name,
    /// Oldest modification time first; ties fall back to full path.
    ModifiedOldest,
    /// Oldest JPEG capture time first; files without capture time go last,
    /// ties fall back to full path.
    CaptureDateOldest,
}

impl SortOrder {
    pub fn label(&self) -> &'static str {
        match self {
            SortOrder::Path => "Path (A–Z)",
            SortOrder::Name => "Name (A–Z)",
            SortOrder::ModifiedOldest => "Modified (oldest first)",
            SortOrder::CaptureDateOldest => "Capture date (oldest first)",
        }
    }
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
    /// Numbering/processing order. Defaults to the historical path order so
    /// journal records written before this field existed still deserialize.
    #[serde(default)]
    pub sort: SortOrder,
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
            sort: SortOrder::Path,
        }
    }
}

/// A hole in a numbered filename sequence within one folder, detected over a
/// plan's proposed names: the pattern was seen at least three times and some
/// numbers in its range never occur. Informational only — a gap can be
/// legitimate (a deleted photo), which is why it is a warning, not an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SequenceGap {
    /// Proposed-name pattern with the varying number replaced by `#`
    /// characters, e.g. `IMG_####.jpg`.
    pub pattern: String,
    /// Width of the varying number, for zero-padded display of `missing`.
    pub width: usize,
    pub folder: PathBuf,
    pub present: usize,
    pub missing_count: usize,
    /// The first missing numbers (capped); `missing_count` is the true total.
    pub missing: Vec<u64>,
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
    SequenceGap,
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
    /// Numbered-sequence holes detected in the proposed names.
    #[serde(default)]
    pub sequence_gaps: Vec<SequenceGap>,
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
