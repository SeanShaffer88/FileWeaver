//! Reviewable plan handoff: export a plan to a portable file, then later —
//! or on another machine — revalidate it against the folder as it exists
//! *now* and apply only the rows that still check out.
//!
//! A plan file is not a backup (it contains no file contents), not a preset
//! (it names specific files), and never code: the rule pipeline inside it is
//! the same closed rule enum the planner already validates. Import never
//! applies anything; it produces a drift report, and the applicable subset
//! is rebuilt through the normal planner so it passes the same validation
//! as any fresh plan.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{
    FileFingerprint, PlanRequest, PlanSummary, PlannedAction, PlannedJob, SequenceGap, StageValue,
};
use crate::planner::{PlanError, plan, subset_plan};
use crate::rules::RulePipeline;

pub const PLAN_FORMAT: &str = "fileweaver-plan";
pub const PLAN_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum HandoffError {
    #[error("this file is not a FileWeaver plan (format tag found: {0:?})")]
    WrongFormat(String),
    #[error("plan format version {0} is not supported by this version of FileWeaver")]
    UnsupportedVersion(u32),
    #[error("the plan file could not be parsed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("the plan could not be revalidated: {0}")]
    Plan(#[from] PlanError),
}

/// One file row of a plan handoff, exactly as proposed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffAction {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub proposed_name: String,
    /// The source's size + modification-time fingerprint when the plan was
    /// made. Drift detection compares the current file against this.
    pub source_fingerprint: FileFingerprint,
    pub stages: Vec<StageValue>,
}

/// The portable plan file (`.fileweaver-plan.json`). The envelope fields are
/// the format commitment; the inner objects reuse the shapes the journal
/// already serializes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanFile {
    pub format: String,
    pub format_version: u32,
    pub app_version: String,
    pub created_unix_ms: i64,
    pub request: PlanRequest,
    pub pipeline: RulePipeline,
    pub actions: Vec<HandoffAction>,
    pub summary: PlanSummary,
    pub sequence_gaps: Vec<SequenceGap>,
}

impl PlanFile {
    /// Assembles a plan file from a built plan. `created_unix_ms` is supplied
    /// by the caller so the core stays clock-free and testable.
    pub fn from_plan(plan: &PlannedJob, pipeline: &RulePipeline, created_unix_ms: i64) -> Self {
        Self {
            format: PLAN_FORMAT.to_string(),
            format_version: PLAN_FORMAT_VERSION,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            created_unix_ms,
            request: plan.request.clone(),
            pipeline: pipeline.clone(),
            actions: plan
                .actions
                .iter()
                .map(|action| HandoffAction {
                    source: action.snapshot.source.clone(),
                    destination: action.destination.clone(),
                    proposed_name: action.proposed_name.clone(),
                    source_fingerprint: action.snapshot.fingerprint.clone(),
                    stages: action.stages.clone(),
                })
                .collect(),
            summary: plan.summary.clone(),
            sequence_gaps: plan.sequence_gaps.clone(),
        }
    }

    pub fn to_json(&self) -> Result<String, HandoffError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Parses a plan file, refusing anything that is not a supported
    /// FileWeaver plan with a specific reason.
    pub fn from_json(text: &str) -> Result<Self, HandoffError> {
        let file: PlanFile = serde_json::from_str(text)?;
        if file.format != PLAN_FORMAT {
            return Err(HandoffError::WrongFormat(file.format));
        }
        if file.format_version != PLAN_FORMAT_VERSION {
            return Err(HandoffError::UnsupportedVersion(file.format_version));
        }
        Ok(file)
    }

    /// Revalidates this plan against the current state of its folder and
    /// returns the row-by-row drift report. Read-only.
    pub fn evaluate(&self) -> Result<DriftReport, HandoffError> {
        let fresh = plan(self.request.clone(), &self.pipeline)?;
        let fresh_by_source: HashMap<String, &PlannedAction> = fresh
            .actions
            .iter()
            .map(|action| (path_key(&action.snapshot.source), action))
            .collect();
        let stored_sources: HashSet<String> =
            self.actions.iter().map(|a| path_key(&a.source)).collect();

        let mut rows = Vec::with_capacity(self.actions.len());
        for (index, stored) in self.actions.iter().enumerate() {
            rows.push(RowDrift {
                index,
                source: stored.source.clone(),
                destination: stored.destination.clone(),
                state: classify(stored, &fresh_by_source, &stored_sources),
                detail: String::new(),
            });
        }
        // Fill in human-readable details now that states are known.
        for row in &mut rows {
            row.detail = detail_for(row.state, &self.actions[row.index]);
        }

        // Files the stored plan knows nothing about. A file sitting at a
        // stored row's destination is not "new" for this purpose — that row
        // already reports it as TargetNowExists.
        let stored_destinations: HashSet<String> = self
            .actions
            .iter()
            .map(|action| path_key(&action.destination))
            .collect();
        let new_files = fresh
            .actions
            .iter()
            .filter(|action| {
                let key = path_key(&action.snapshot.source);
                !stored_sources.contains(&key) && !stored_destinations.contains(&key)
            })
            .map(|action| action.snapshot.source.clone())
            .collect();

        Ok(DriftReport { rows, new_files })
    }
}

/// How one stored row compares with the folder as it exists now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftState {
    /// Source unchanged, target still free, fresh plan proposes the same name.
    Unchanged,
    /// Source exists but its size or modification time differs.
    SourceChanged,
    /// Source no longer exists (or cannot be read).
    SourceMissing,
    /// The fresh plan now proposes a different name for this source.
    ResultDiffers,
    /// Something the plan did not put there now sits at the destination.
    TargetNowExists,
}

#[derive(Debug, Clone)]
pub struct RowDrift {
    pub index: usize,
    pub source: PathBuf,
    pub destination: PathBuf,
    pub state: DriftState,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct DriftReport {
    pub rows: Vec<RowDrift>,
    /// Files the fresh plan covers that the stored plan never mentioned.
    pub new_files: Vec<PathBuf>,
}

impl DriftReport {
    pub fn count(&self, state: DriftState) -> usize {
        self.rows.iter().filter(|row| row.state == state).count()
    }

    /// Builds the applicable subset as a fresh, fully validated plan: the
    /// fresh plan's actions for exactly the unchanged rows. Returns `None`
    /// when nothing can be applied — no unchanged rows, or the subset fails
    /// validation as a whole (for example, a new file now collides with a
    /// survivor's target).
    pub fn applicable_plan(&self, file: &PlanFile) -> Result<Option<PlannedJob>, HandoffError> {
        let clean: HashSet<String> = self
            .rows
            .iter()
            .filter(|row| row.state == DriftState::Unchanged)
            .map(|row| path_key(&row.source))
            .collect();
        if clean.is_empty() {
            return Ok(None);
        }
        let fresh = plan(file.request.clone(), &file.pipeline)?;
        let subset = subset_plan(&fresh, |action| {
            clean.contains(&path_key(&action.snapshot.source))
        });
        Ok(subset.is_executable().then_some(subset))
    }
}

fn classify(
    stored: &HandoffAction,
    fresh_by_source: &HashMap<String, &PlannedAction>,
    stored_sources: &HashSet<String>,
) -> DriftState {
    let metadata = match fs::metadata(&stored.source) {
        Ok(metadata) => metadata,
        Err(_) => return DriftState::SourceMissing,
    };
    if FileFingerprint::from_metadata(&metadata) != stored.source_fingerprint {
        return DriftState::SourceChanged;
    }
    match fresh_by_source.get(&path_key(&stored.source)) {
        Some(fresh) if fresh.proposed_name != stored.proposed_name => {
            return DriftState::ResultDiffers;
        }
        Some(_) => {}
        // Not covered by the fresh plan at all (for example it became a
        // symlink, which the planner deliberately skips): the stored result
        // can no longer be reproduced.
        None => return DriftState::ResultDiffers,
    }
    if stored.destination != stored.source
        && stored.destination.exists()
        && !stored_sources.contains(&path_key(&stored.destination))
    {
        return DriftState::TargetNowExists;
    }
    DriftState::Unchanged
}

fn detail_for(state: DriftState, stored: &HandoffAction) -> String {
    match state {
        DriftState::Unchanged => "Still matches the plan exactly.".into(),
        DriftState::SourceChanged => format!(
            "{} changed since this plan was made (size or modification time differs).",
            stored.source.display()
        ),
        DriftState::SourceMissing => format!("{} no longer exists.", stored.source.display()),
        DriftState::ResultDiffers => format!(
            "The rules now produce a different name for {} than this plan recorded.",
            stored.source.display()
        ),
        DriftState::TargetNowExists => format!(
            "{} already exists and was not part of this plan.",
            stored.destination.display()
        ),
    }
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SortOrder;
    use crate::rules::Rule;
    use std::fs;
    use tempfile::tempdir;

    fn plan_folder(dir: &Path, names: &[&str], rules: Vec<Rule>) -> (PlannedJob, RulePipeline) {
        for name in names {
            fs::write(dir.join(name), format!("content of {name}")).unwrap();
        }
        let request = PlanRequest::rename(vec![dir.to_path_buf()]);
        let pipeline = RulePipeline { rules };
        let planned = plan(request, &pipeline).unwrap();
        (planned, pipeline)
    }

    fn prefix_rules() -> Vec<Rule> {
        vec![Rule::AddPrefix {
            text: "2026-".into(),
        }]
    }

    fn state_of(report: &DriftReport, file_name: &str) -> DriftState {
        report
            .rows
            .iter()
            .find(|row| {
                row.source
                    .file_name()
                    .map(|name| name.to_string_lossy() == file_name)
                    .unwrap_or(false)
            })
            .unwrap_or_else(|| panic!("no row for {file_name}"))
            .state
    }

    #[test]
    fn roundtrip_and_clean_evaluation() {
        let dir = tempdir().unwrap();
        let (planned, pipeline) = plan_folder(dir.path(), &["a.txt", "b.txt"], prefix_rules());
        let file = PlanFile::from_plan(&planned, &pipeline, 42);
        let parsed = PlanFile::from_json(&file.to_json().unwrap()).unwrap();
        assert_eq!(parsed.format, PLAN_FORMAT);
        assert_eq!(parsed.created_unix_ms, 42);
        assert_eq!(parsed.actions.len(), 2);

        let report = parsed.evaluate().unwrap();
        assert_eq!(report.count(DriftState::Unchanged), 2);
        assert!(report.new_files.is_empty());
        let applicable = report.applicable_plan(&parsed).unwrap().unwrap();
        assert_eq!(applicable.actions.len(), 2);
        assert_eq!(applicable.actions[0].proposed_name, "2026-a.txt");
        assert!(applicable.is_executable());
    }

    #[test]
    fn drift_states_are_classified_per_row() {
        let dir = tempdir().unwrap();
        let names = [
            "photo_01.txt",
            "photo_02.txt",
            "photo_03.txt",
            "photo_04.txt",
            "photo_05.txt",
            "photo_06.txt",
        ];
        let (planned, pipeline) = plan_folder(dir.path(), &names, prefix_rules());
        let file = PlanFile::from_plan(&planned, &pipeline, 7);

        // Four independent changes after the plan was made.
        fs::write(dir.path().join("photo_02.txt"), "edited, and longer").unwrap();
        fs::remove_file(dir.path().join("photo_03.txt")).unwrap();
        fs::write(dir.path().join("2026-photo_04.txt"), "intruder").unwrap();
        fs::write(dir.path().join("new.txt"), "unplanned").unwrap();

        let report = file.evaluate().unwrap();
        assert_eq!(state_of(&report, "photo_01.txt"), DriftState::Unchanged);
        assert_eq!(state_of(&report, "photo_02.txt"), DriftState::SourceChanged);
        assert_eq!(state_of(&report, "photo_03.txt"), DriftState::SourceMissing);
        assert_eq!(
            state_of(&report, "photo_04.txt"),
            DriftState::TargetNowExists
        );
        assert_eq!(state_of(&report, "photo_05.txt"), DriftState::Unchanged);
        assert_eq!(state_of(&report, "photo_06.txt"), DriftState::Unchanged);
        assert_eq!(report.new_files, vec![dir.path().join("new.txt")]);

        let applicable = report.applicable_plan(&file).unwrap().unwrap();
        let kept: Vec<&str> = applicable
            .actions
            .iter()
            .map(|action| action.snapshot.original_name.as_str())
            .collect();
        assert_eq!(kept, ["photo_01.txt", "photo_05.txt", "photo_06.txt"]);
    }

    #[test]
    fn counter_shift_makes_results_differ() {
        let dir = tempdir().unwrap();
        let rules = vec![Rule::AppendCounter {
            separator: "_".into(),
            start_at: 1,
            width: 4,
        }];
        let (planned, pipeline) =
            plan_folder(dir.path(), &["p1.txt", "p2.txt", "p3.txt", "p4.txt"], rules);
        let file = PlanFile::from_plan(&planned, &pipeline, 9);

        // A new file that sorts first shifts every counter.
        fs::write(dir.path().join("p0.txt"), "early").unwrap();

        let report = file.evaluate().unwrap();
        assert_eq!(report.count(DriftState::ResultDiffers), 4);
        assert_eq!(report.new_files, vec![dir.path().join("p0.txt")]);
        assert!(report.applicable_plan(&file).unwrap().is_none());
    }

    #[test]
    fn format_guards_refuse_foreign_and_future_files() {
        let dir = tempdir().unwrap();
        let (planned, pipeline) = plan_folder(dir.path(), &["a.txt"], prefix_rules());
        let file = PlanFile::from_plan(&planned, &pipeline, 1);
        let mut value: serde_json::Value = serde_json::from_str(&file.to_json().unwrap()).unwrap();

        value["format"] = serde_json::json!("someone-elses-format");
        assert!(matches!(
            PlanFile::from_json(&value.to_string()),
            Err(HandoffError::WrongFormat(_))
        ));

        value["format"] = serde_json::json!(PLAN_FORMAT);
        value["format_version"] = serde_json::json!(99);
        assert!(matches!(
            PlanFile::from_json(&value.to_string()),
            Err(HandoffError::UnsupportedVersion(99))
        ));

        assert!(matches!(
            PlanFile::from_json("not json at all"),
            Err(HandoffError::Json(_))
        ));
    }

    #[test]
    fn old_plan_files_without_sort_still_parse() {
        // PlanRequest.sort is serde-defaulted, so a request object written
        // before the field existed must still load inside a plan file.
        let dir = tempdir().unwrap();
        let (planned, pipeline) = plan_folder(dir.path(), &["a.txt"], prefix_rules());
        let file = PlanFile::from_plan(&planned, &pipeline, 1);
        let mut value: serde_json::Value = serde_json::from_str(&file.to_json().unwrap()).unwrap();
        value["request"].as_object_mut().unwrap().remove("sort");
        let parsed = PlanFile::from_json(&value.to_string()).unwrap();
        assert_eq!(parsed.request.sort, SortOrder::Path);
    }
}
