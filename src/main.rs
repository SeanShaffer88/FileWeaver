#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::env;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;

use fileweaver_core::{
    CancellationToken, CollisionPolicy, ExecuteProgress, Executor, Journal, OperationKind,
    PlanRequest, PlannedJob, Rule, RulePipeline, plan,
};
use slint::{ComponentHandle, ModelRc, VecModel};

slint::include_modules!();

#[derive(Default)]
struct AppState {
    plan: Mutex<Option<PlannedJob>>,
    cancellation: Mutex<Option<CancellationToken>>,
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
