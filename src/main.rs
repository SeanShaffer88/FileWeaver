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

    ui.run()
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
    let request = PlanRequest {
        inputs: vec![PathBuf::from(&folder)],
        operation: OperationKind::Rename,
        destination_root: None,
        recursive: ui.get_recursive(),
        include_directories: false,
        collision_policy: CollisionPolicy::Block,
        allow_legacy_long_paths: false,
    };
    ui.set_can_apply(false);
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
            let executable = new_plan.is_executable();
            if let Ok(mut current) = state.plan.lock() {
                *current = Some(new_plan);
            }
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_rows(ModelRc::new(Rc::new(VecModel::from(rows))));
                ui.set_status(summary.into());
                ui.set_can_apply(executable);
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

    let cancellation = CancellationToken::default();
    if let Ok(mut current) = state.cancellation.lock() {
        *current = Some(cancellation.clone());
    }
    if let Some(ui) = weak.upgrade() {
        ui.set_can_apply(false);
        ui.set_status(
            "Applying reviewed changes. The app records a local recovery ledger as it works…"
                .into(),
        );
    }

    thread::spawn(move || {
        let result = (|| {
            let journal = Journal::open(operation_ledger_path())?;
            let mut executor = Executor::new(journal);
            executor.execute(&plan, &cancellation, |event| update_progress(&weak, event))
        })();
        let _ = weak.upgrade_in_event_loop(move |ui| match result {
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

fn operation_ledger_path() -> PathBuf {
    let base = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_STATE_HOME").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("FileWeaver").join("operations.sqlite")
}
