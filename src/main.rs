#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::env;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;

use fileweaver_core::{
    CancellationToken, CollisionPolicy, Divergence, ExecuteError, ExecuteProgress, Executor,
    JobReview, JobSummary, Journal, JournalError, OperationKind, PlanRequest, PlannedJob,
    RollbackReport, Rule, RulePipeline, plan,
};
use slint::{ComponentHandle, ModelRc, VecModel};

slint::include_modules!();

#[derive(Default)]
struct AppState {
    plan: Mutex<Option<PlannedJob>>,
    cancellation: Mutex<Option<CancellationToken>>,
    ledger_job_ids: Mutex<Vec<i64>>,
    selected_job_id: Mutex<Option<i64>>,
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;
    let state = Arc::new(AppState::default());
    let weak = ui.as_weak();

    ui.on_scan_folder({
        let state = Arc::clone(&state);
        move |folder| schedule_preview(weak.clone(), Arc::clone(&state), folder.to_string(), false)
    });

    ui.on_refresh_preview({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                schedule_preview(
                    ui.as_weak(),
                    Arc::clone(&state),
                    ui.get_folder_path().to_string(),
                    true,
                );
            }
        }
    });

    ui.on_apply_changes({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move || schedule_apply(weak.clone(), Arc::clone(&state))
    });

    ui.on_stop_apply({
        let state = Arc::clone(&state);
        move || {
            if let Ok(guard) = state.cancellation.lock()
                && let Some(token) = &*guard
            {
                token.cancel();
            }
        }
    });

    ui.on_refresh_ledger({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move || refresh_ledger(weak.clone(), Arc::clone(&state))
    });

    ui.on_select_job({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move |index| {
            select_job(
                weak.clone(),
                Arc::clone(&state),
                usize::try_from(index).unwrap_or(usize::MAX),
            )
        }
    });

    ui.on_rollback_selected({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move || schedule_rollback(weak.clone(), Arc::clone(&state))
    });

    // Initial history load is synchronous: it is a small bounded read, and
    // the event loop is not running yet to receive a posted update.
    match Journal::open(operation_ledger_path()).and_then(|journal| journal.list_jobs(20)) {
        Ok(jobs) => {
            if let Ok(mut ids) = state.ledger_job_ids.lock() {
                *ids = jobs.iter().map(|job| job.id).collect();
            }
            apply_ledger_jobs(&ui, &jobs);
        }
        Err(error) => {
            ui.set_ledger_detail(format!("Could not read the operation ledger: {error}").into());
        }
    }

    ui.on_inputs_changed({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move || {
            // Any edit to the folder or rules invalidates the stored preview:
            // the plan it produced must never be applied as if it were current.
            let had_plan = match state.plan.lock() {
                Ok(mut guard) => guard.take().is_some(),
                Err(_) => false,
            };
            if had_plan && let Some(ui) = weak.upgrade() {
                ui.set_can_apply(false);
                ui.set_risk_budget("".into());
                ui.set_status(
                    "Settings changed since the preview. Select Refresh preview to review the updated plan before applying."
                        .into(),
                );
            }
        }
    });

    ui.run()
}

fn request_from_ui(ui: &MainWindow) -> PlanRequest {
    PlanRequest {
        inputs: vec![PathBuf::from(ui.get_folder_path().trim())],
        operation: OperationKind::Rename,
        destination_root: None,
        recursive: ui.get_recursive(),
        include_directories: false,
        collision_policy: CollisionPolicy::Block,
        allow_legacy_long_paths: false,
    }
}

/// True when `request` and `pipeline` are exactly the inputs that produced
/// `plan`. Compared via serialization so the check cannot drift from what the
/// planner and journal actually record.
fn inputs_match(plan: &PlannedJob, request: &PlanRequest, pipeline: &RulePipeline) -> bool {
    let request_json = serde_json::to_string(&plan.request).ok();
    let current_json = serde_json::to_string(request).ok();
    let pipeline_json = serde_json::to_string_pretty(pipeline).ok();
    request_json.is_some()
        && request_json == current_json
        && pipeline_json.as_deref() == Some(plan.pipeline_json.as_str())
}

fn plan_is_current(plan: &PlannedJob, ui: &MainWindow) -> bool {
    inputs_match(plan, &request_from_ui(ui), &pipeline_from_ui(ui))
}

fn schedule_preview(
    weak: slint::Weak<MainWindow>,
    state: Arc<AppState>,
    folder: String,
    use_current_rules: bool,
) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    let folder = folder.trim().to_owned();
    if folder.is_empty() {
        ui.set_status("Enter a folder path before scanning.".into());
        return;
    }

    let pipeline = pipeline_from_ui(&ui);
    let request = request_from_ui(&ui);
    ui.set_can_apply(false);
    ui.set_risk_budget("".into());
    ui.set_status(
        if use_current_rules {
            "Refreshing the non-destructive preview…"
        } else {
            "Scanning locally and building a non-destructive preview…"
        }
        .into(),
    );

    thread::spawn(move || match plan(request, &pipeline) {
        Ok(new_plan) => {
            let rows = rows_for_plan(&new_plan);
            let summary = summary_text(&new_plan);
            let risk_budget = risk_budget_text(&new_plan);
            let executable = new_plan.is_executable();
            let plan_for_check = new_plan.clone();
            if let Ok(mut current) = state.plan.lock() {
                *current = Some(new_plan);
            }
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_rows(ModelRc::new(Rc::new(VecModel::from(rows))));
                // The inputs may have changed while this preview was being
                // built on the worker thread; only present the plan as
                // applicable if it still matches what the user now sees.
                // (Apply re-validates regardless, as a final guard.)
                if plan_is_current(&plan_for_check, &ui) {
                    ui.set_status(summary.into());
                    ui.set_risk_budget(risk_budget.into());
                    ui.set_can_apply(executable);
                } else {
                    ui.set_status(
                        "Settings changed while the preview was building. Select Refresh preview to review the updated plan before applying."
                            .into(),
                    );
                    ui.set_risk_budget("".into());
                    ui.set_can_apply(false);
                }
            });
        }
        Err(error) => {
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_status(format!("Could not build preview: {error}").into());
                ui.set_can_apply(false);
            });
        }
    });
}

fn schedule_apply(weak: slint::Weak<MainWindow>, state: Arc<AppState>) {
    let plan = match state.plan.lock() {
        Ok(guard) => guard.clone(),
        Err(_) => None,
    };
    let Some(plan) = plan else {
        if let Some(ui) = weak.upgrade() {
            ui.set_status("Create and review a preview before applying changes.".into());
        }
        return;
    };
    if !plan.is_executable() {
        if let Some(ui) = weak.upgrade() {
            ui.set_status("Resolve blocking preview errors before applying changes.".into());
        }
        return;
    }
    // Final guard: never execute a plan whose folder or rules no longer match
    // what the user sees, even if an input change somehow reached Apply
    // without invalidating the preview first.
    if let Some(ui) = weak.upgrade()
        && !plan_is_current(&plan, &ui)
    {
        if let Ok(mut guard) = state.plan.lock() {
            *guard = None;
        }
        ui.set_can_apply(false);
        ui.set_risk_budget("".into());
        ui.set_status(
            "The folder or rules changed since this preview. Refresh the preview and review it before applying."
                .into(),
        );
        return;
    }

    let cancellation = CancellationToken::default();
    if let Ok(mut current) = state.cancellation.lock() {
        *current = Some(cancellation.clone());
    }
    if let Some(ui) = weak.upgrade() {
        ui.set_can_apply(false);
        ui.set_applying(true);
        ui.set_status(
            "Applying reviewed changes. The app records a local recovery ledger as it works…"
                .into(),
        );
    }

    let state_for_finish = Arc::clone(&state);
    thread::spawn(move || {
        let result = (|| {
            let journal = Journal::open(operation_ledger_path())?;
            let mut executor = Executor::new(journal);
            executor.execute(&plan, &cancellation, |event| update_progress(&weak, event))
        })();
        // The job has changed the folder, so the preview is consumed: a fresh
        // scan is required before anything else can be applied.
        if let Ok(mut guard) = state_for_finish.plan.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = state_for_finish.cancellation.lock() {
            *guard = None;
        }
        refresh_ledger(weak.clone(), Arc::clone(&state_for_finish));
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_applying(false);
            ui.set_can_apply(false);
            ui.set_risk_budget("".into());
            match result {
            Ok(outcome) if outcome.cancelled => ui.set_status(
                format!(
                    "Stopped safely after {} changes. Completed items remain recorded for recovery.",
                    outcome.applied
                )
                .into(),
            ),
            Ok(outcome) if outcome.failed > 0 => ui.set_status(
                format!(
                    "{} changes completed; {} failed. Review the operation ledger before retrying.",
                    outcome.applied, outcome.failed
                )
                .into(),
            ),
            Ok(outcome) => ui.set_status(
                format!(
                    "Completed {} changes. Job {} is stored locally for a future recovery review.",
                    outcome.applied, outcome.job_id
                )
                .into(),
            ),
            Err(error) => ui.set_status(format!("No changes were started: {error}").into()),
            }
        });
    });
}

fn update_progress(weak: &slint::Weak<MainWindow>, event: ExecuteProgress) {
    let weak = weak.clone();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        ui.set_status(
            format!(
                "{}: {}/{} — {}",
                event.message,
                event.completed,
                event.total,
                event.current_path.display()
            )
            .into(),
        );
    });
}

fn pipeline_from_ui(ui: &MainWindow) -> RulePipeline {
    let mut rules = Vec::new();
    if !ui.get_prefix().is_empty() {
        rules.push(Rule::AddPrefix {
            text: ui.get_prefix().to_string(),
        });
    }
    if !ui.get_find_text().is_empty() {
        rules.push(Rule::ReplaceText {
            find: ui.get_find_text().to_string(),
            replacement: ui.get_replace_text().to_string(),
            replace_all: true,
        });
    }
    if ui.get_add_counter() {
        rules.push(Rule::AppendCounter {
            separator: "_".into(),
            start_at: 1,
            width: 4,
        });
    }
    RulePipeline { rules }
}

fn rows_for_plan(plan: &PlannedJob) -> Vec<PreviewRow> {
    plan.actions
        .iter()
        .map(|action| PreviewRow {
            old_name: action.snapshot.original_name.clone().into(),
            proposed_name: action.proposed_name.clone().into(),
            state: row_state(action).into(),
        })
        .collect()
}

fn row_state(action: &fileweaver_core::PlannedAction) -> String {
    if let Some(issue) = action
        .issues
        .iter()
        .find(|issue| issue.severity == fileweaver_core::Severity::Error)
    {
        format!("Blocked: {}", issue.message)
    } else if !action.will_change() {
        "No change".into()
    } else if let Some(issue) = action
        .issues
        .iter()
        .find(|issue| issue.severity == fileweaver_core::Severity::Warning)
    {
        format!("Review: {}", issue.message)
    } else {
        "Ready".into()
    }
}

fn summary_text(plan: &PlannedJob) -> String {
    let summary = &plan.summary;
    let extra = if summary.cross_volume_candidates > 0 {
        format!(
            " {} may cross a volume; those moves are copy-then-remove and are not atomic.",
            summary.cross_volume_candidates
        )
    } else {
        String::new()
    };
    if summary.blocking_errors > 0 {
        format!(
            "Preview: {} files discovered; {} ready; {} blocking issue(s). Apply remains disabled.{}",
            summary.discovered, summary.ready, summary.blocking_errors, extra
        )
    } else {
        format!(
            "Preview: {} files discovered; {} ready; {} unchanged; {} warning(s).{}",
            summary.discovered, summary.ready, summary.unchanged, summary.warnings, extra
        )
    }
}

fn risk_budget_text(plan: &PlannedJob) -> String {
    let has_error = |action: &fileweaver_core::PlannedAction| {
        action
            .issues
            .iter()
            .any(|issue| issue.severity == fileweaver_core::Severity::Error)
    };
    let blocked = plan.actions.iter().filter(|a| has_error(a)).count();
    let review = plan
        .actions
        .iter()
        .filter(|a| {
            a.will_change()
                && !has_error(a)
                && a.issues
                    .iter()
                    .any(|issue| issue.severity == fileweaver_core::Severity::Warning)
        })
        .count();
    let mut text = format!(
        "Risk budget: {} to change · {} blocked · {} to review · {} unchanged · 0 overwrites — FileWeaver never overwrites.",
        plan.summary.ready, blocked, review, plan.summary.unchanged
    );
    if plan.summary.cross_volume_candidates > 0 {
        text.push_str(&format!(
            " {} cross-volume move(s) would be copy-then-remove and not atomic.",
            plan.summary.cross_volume_candidates
        ));
    }
    text
}

fn kind_label(kind: OperationKind) -> &'static str {
    match kind {
        OperationKind::Rename => "Rename",
        OperationKind::Move => "Move",
        OperationKind::Copy => "Copy",
    }
}

fn format_unix_ms(ms: i64) -> String {
    let nanos = i128::from(ms) * 1_000_000;
    match time::OffsetDateTime::from_unix_timestamp_nanos(nanos) {
        Ok(moment) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02} UTC",
            moment.year(),
            u8::from(moment.month()),
            moment.day(),
            moment.hour(),
            moment.minute()
        ),
        Err(_) => "unknown time".into(),
    }
}

fn ledger_row_for(job: &JobSummary) -> LedgerRow {
    let mut detail = format!("{} · {} applied", job.status, job.applied_items);
    if job.failed_items > 0 {
        detail.push_str(&format!(" · {} failed", job.failed_items));
    }
    if job.rolled_back_items > 0 {
        detail.push_str(&format!(" · {} rolled back", job.rolled_back_items));
    }
    if let Some(message) = &job.message {
        detail.push_str(&format!(" · {message}"));
    }
    LedgerRow {
        heading: format!(
            "#{} · {} · {}",
            job.id,
            kind_label(job.operation),
            format_unix_ms(job.started_unix_ms)
        )
        .into(),
        detail: detail.into(),
    }
}

fn apply_ledger_jobs(ui: &MainWindow, jobs: &[JobSummary]) {
    let rows: Vec<LedgerRow> = jobs.iter().map(ledger_row_for).collect();
    ui.set_ledger_rows(ModelRc::new(Rc::new(VecModel::from(rows))));
    ui.set_selected_job_index(-1);
    ui.set_can_rollback(false);
    ui.set_confirming_rollback(false);
    ui.set_ledger_detail(
        if jobs.is_empty() {
            "No jobs recorded yet. Jobs you apply will appear here for review and rollback."
        } else {
            "Select a job to review how its files look now."
        }
        .into(),
    );
}

fn refresh_ledger(weak: slint::Weak<MainWindow>, state: Arc<AppState>) {
    thread::spawn(move || {
        let result: Result<Vec<JobSummary>, JournalError> = (|| {
            let journal = Journal::open(operation_ledger_path())?;
            journal.list_jobs(20)
        })();
        match result {
            Ok(jobs) => {
                if let Ok(mut ids) = state.ledger_job_ids.lock() {
                    *ids = jobs.iter().map(|job| job.id).collect();
                }
                if let Ok(mut selected) = state.selected_job_id.lock() {
                    *selected = None;
                }
                let _ = weak.upgrade_in_event_loop(move |ui| apply_ledger_jobs(&ui, &jobs));
            }
            Err(error) => {
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_ledger_detail(
                        format!("Could not read the operation ledger: {error}").into(),
                    );
                });
            }
        }
    });
}

fn select_job(weak: slint::Weak<MainWindow>, state: Arc<AppState>, index: usize) {
    let job_id = state
        .ledger_job_ids
        .lock()
        .ok()
        .and_then(|ids| ids.get(index).copied());
    let Some(job_id) = job_id else {
        return;
    };
    if let Ok(mut selected) = state.selected_job_id.lock() {
        *selected = Some(job_id);
    }
    if let Some(ui) = weak.upgrade() {
        ui.set_selected_job_index(i32::try_from(index).unwrap_or(-1));
        ui.set_confirming_rollback(false);
        ui.set_can_rollback(false);
        ui.set_ledger_detail("Reviewing this job against the current files…".into());
    }

    thread::spawn(move || {
        let result: Result<JobReview, ExecuteError> = (|| {
            let journal = Journal::open(operation_ledger_path())?;
            Executor::new(journal).review(job_id)
        })();
        let _ = weak.upgrade_in_event_loop(move |ui| match result {
            Ok(review) => {
                ui.set_ledger_detail(review_detail_text(&review).into());
                ui.set_can_rollback(review.safely_reversible() > 0);
            }
            Err(error) => {
                ui.set_ledger_detail(format!("Could not review job #{job_id}: {error}").into());
                ui.set_can_rollback(false);
            }
        });
    });
}

fn review_detail_text(review: &JobReview) -> String {
    let total = review.items.len();
    if total == 0 {
        return format!(
            "Job #{} ({}): no applied files remain to review — already rolled back, or nothing was applied.",
            review.job_id,
            kind_label(review.operation).to_lowercase()
        );
    }
    let occupied = review
        .items
        .iter()
        .filter(|item| item.original_occupied)
        .count();
    let mut text = format!(
        "Job #{} ({}): {} file(s) checked — {} unchanged, {} changed since the job, {} missing.",
        review.job_id,
        kind_label(review.operation).to_lowercase(),
        total,
        review.count(Divergence::Intact),
        review.count(Divergence::Changed),
        review.count(Divergence::Missing)
    );
    if occupied > 0 {
        text.push_str(&format!(
            " {occupied} original location(s) are occupied again."
        ));
    }
    let reversible = review.safely_reversible();
    if reversible > 0 {
        text.push_str(&format!(
            " Rollback can safely restore {reversible} file(s); changed or missing files are never overwritten."
        ));
    } else {
        text.push_str(" Nothing in this job can be safely rolled back.");
    }
    text
}

fn schedule_rollback(weak: slint::Weak<MainWindow>, state: Arc<AppState>) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    if !ui.get_confirming_rollback() {
        ui.set_confirming_rollback(true);
        ui.set_status(
            "Confirm rollback: unchanged files get their original names back, and changed or missing files are never overwritten. Select Roll back again to proceed."
                .into(),
        );
        return;
    }
    ui.set_confirming_rollback(false);
    let job_id = state
        .selected_job_id
        .lock()
        .ok()
        .and_then(|selected| *selected);
    let Some(job_id) = job_id else {
        ui.set_status("Select a job in the recovery ledger first.".into());
        return;
    };
    ui.set_can_rollback(false);
    ui.set_status(format!("Rolling back job #{job_id}…").into());

    thread::spawn(move || {
        let result: Result<RollbackReport, ExecuteError> = (|| {
            let journal = Journal::open(operation_ledger_path())?;
            let mut executor = Executor::new(journal);
            executor.rollback(job_id)
        })();
        refresh_ledger(weak.clone(), Arc::clone(&state));
        let _ = weak.upgrade_in_event_loop(move |ui| match result {
            Ok(report) => {
                let first_skip = report
                    .skipped
                    .first()
                    .map(|item| {
                        format!(
                            " First skipped: {} — {}",
                            item.destination.display(),
                            item.reason
                        )
                    })
                    .unwrap_or_default();
                ui.set_status(
                    format!(
                        "Rollback of job #{}: {} restored, {} copied file(s) removed, {} skipped.{}",
                        job_id,
                        report.restored,
                        report.removed_copies,
                        report.skipped.len(),
                        first_skip
                    )
                    .into(),
                );
            }
            Err(error) => {
                ui.set_status(format!("Rollback of job #{job_id} failed: {error}").into());
            }
        });
    });
}

fn operation_ledger_path() -> PathBuf {
    let base = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_STATE_HOME").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("FileWeaver").join("operations.sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sample_plan(dir: &std::path::Path) -> (PlanRequest, RulePipeline, PlannedJob) {
        fs::write(dir.join("photo.jpg"), b"x").unwrap();
        let request = PlanRequest {
            inputs: vec![dir.to_path_buf()],
            operation: OperationKind::Rename,
            destination_root: None,
            recursive: false,
            include_directories: false,
            collision_policy: CollisionPolicy::Block,
            allow_legacy_long_paths: false,
        };
        let pipeline = RulePipeline {
            rules: vec![Rule::AddPrefix {
                text: "2026-".into(),
            }],
        };
        let planned = plan(request.clone(), &pipeline).unwrap();
        (request, pipeline, planned)
    }

    #[test]
    fn matching_inputs_are_current() {
        let dir = tempfile::tempdir().unwrap();
        let (request, pipeline, planned) = sample_plan(dir.path());
        assert!(inputs_match(&planned, &request, &pipeline));
    }

    #[test]
    fn changed_rule_invalidates_plan() {
        let dir = tempfile::tempdir().unwrap();
        let (request, _pipeline, planned) = sample_plan(dir.path());
        let changed = RulePipeline {
            rules: vec![Rule::AddPrefix {
                text: "2027-".into(),
            }],
        };
        assert!(!inputs_match(&planned, &request, &changed));
    }

    #[test]
    fn changed_request_invalidates_plan() {
        let dir = tempfile::tempdir().unwrap();
        let (mut request, pipeline, planned) = sample_plan(dir.path());
        request.recursive = true;
        assert!(!inputs_match(&planned, &request, &pipeline));
    }
}
