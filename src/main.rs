#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;

use fileweaver_core::domain::StageValue;
use fileweaver_core::{
    CancellationToken, CollisionPolicy, Divergence, DriftReport, DriftState, ExecuteError,
    ExecuteProgress, Executor, HandoffAction, JobReview, JobSummary, Journal, JournalError,
    OperationKind, PlanFile, PlanRequest, PlannedJob, RollbackReport, Rule, RulePipeline,
    SortOrder, plan,
};
use slint::{ComponentHandle, ModelRc, VecModel};

mod cli;
mod win32;

slint::include_modules!();

/// An opened plan handoff. While this is set the window is in "as proposed"
/// mode and Apply means "apply the handoff's clean subset". Only the plan
/// file is kept: the drift report shown at open time is deliberately not
/// retained, because Apply re-checks the folder at click time rather than
/// trusting a report that may already be stale.
struct HandoffView {
    file: PlanFile,
}

#[derive(Default)]
struct AppState {
    plan: Mutex<Option<PlannedJob>>,
    /// The pipeline that produced `plan`. Stored alongside it so a plan
    /// rebuilt from a handoff's stored rules can be currency-checked even
    /// when the window's rule fields cannot express that pipeline.
    plan_pipeline: Mutex<Option<RulePipeline>>,
    handoff: Mutex<Option<HandoffView>>,
    cancellation: Mutex<Option<CancellationToken>>,
    ledger_job_ids: Mutex<Vec<i64>>,
    selected_job_id: Mutex<Option<i64>>,
}

fn main() -> Result<(), slint::PlatformError> {
    // Command-line maintenance commands run before any window exists and
    // exit with their own status code (0/1/2, see `cli::USAGE`). No
    // arguments — or a first argument that is not a flag — starts the GUI.
    let args: Vec<String> = env::args().skip(1).collect();
    if !args.is_empty() {
        // The release build uses the Windows subsystem (no console of its
        // own), so attach to the parent console to make CLI output visible.
        win32::attach_parent_console();
    }
    match cli::parse_args(&args) {
        cli::ParseOutcome::Gui => {}
        cli::ParseOutcome::Command(command) => {
            std::process::exit(cli::run(command, &purge_ledger_path()));
        }
        cli::ParseOutcome::UsageError(message) => {
            eprintln!("fileweaver: {message}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    }

    // Single instance (Contract §3 #12): a second copy would share the
    // operation ledger with no coordination, so it steps aside instead.
    // The guard is held for the whole GUI run.
    let Some(_instance_guard) = win32::acquire_single_instance() else {
        win32::attach_parent_console();
        eprintln!("FileWeaver is already running.");
        return Ok(());
    };

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

    ui.on_select_row({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move |index| {
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            // In handoff mode the rows are the stored plan's, so provenance
            // comes from the stored stages; otherwise from the live plan.
            let text = state
                .handoff
                .lock()
                .ok()
                .and_then(|guard| {
                    guard
                        .as_ref()
                        .and_then(|view| view.file.actions.get(index))
                        .map(provenance_for_handoff)
                })
                .or_else(|| {
                    state.plan.lock().ok().and_then(|guard| {
                        guard
                            .as_ref()
                            .and_then(|plan| plan.actions.get(index))
                            .map(provenance_text)
                    })
                });
            if let (Some(ui), Some(text)) = (weak.upgrade(), text) {
                ui.set_selected_row_index(index as i32);
                ui.set_provenance(text.into());
            }
        }
    });

    ui.on_save_plan({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move || schedule_save_plan(weak.clone(), Arc::clone(&state))
    });

    ui.on_open_plan({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move || schedule_open_plan(weak.clone(), Arc::clone(&state))
    });

    ui.on_close_handoff({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move || close_handoff(weak.clone(), Arc::clone(&state))
    });

    ui.on_rebuild_handoff({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move || schedule_rebuild_handoff(weak.clone(), Arc::clone(&state))
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
    // the event loop is not running yet to receive a posted update. The
    // open also runs the startup recovery sweep: jobs a previous session
    // left 'running' (it died mid-flight) are marked interrupted.
    let initial = Journal::open(operation_ledger_path()).and_then(|journal| {
        let interrupted = journal.reconcile_interrupted_jobs()?;
        journal.list_jobs(20).map(|jobs| (jobs, interrupted))
    });
    match initial {
        Ok((jobs, interrupted)) => {
            if let Ok(mut ids) = state.ledger_job_ids.lock() {
                *ids = jobs.iter().map(|job| job.id).collect();
            }
            apply_ledger_jobs(&ui, &jobs);
            if interrupted > 0 {
                ui.set_ledger_detail(
                    format!(
                        "{interrupted} job(s) were left unfinished by a previous session and have been marked interrupted. Select one to review how its files look now before rolling anything back."
                    )
                    .into(),
                );
            }
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
            if let Ok(mut guard) = state.plan_pipeline.lock() {
                *guard = None;
            }
            if had_plan && let Some(ui) = weak.upgrade() {
                ui.set_can_apply(false);
                ui.set_can_save_plan(false);
                ui.set_risk_budget("".into());
                ui.set_order_summary("".into());
                ui.set_selected_row_index(-1);
                ui.set_provenance("".into());
                ui.set_status(
                    "Settings changed since the preview. Select Refresh preview to review the updated plan before applying."
                        .into(),
                );
            }
        }
    });

    ui.run()
}

fn sort_from_index(index: i32) -> SortOrder {
    match index {
        1 => SortOrder::Name,
        2 => SortOrder::ModifiedOldest,
        3 => SortOrder::CaptureDateOldest,
        _ => SortOrder::Path,
    }
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
        sort: sort_from_index(ui.get_sort_index()),
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

fn plan_is_current(
    plan: &PlannedJob,
    ui: &MainWindow,
    stored_pipeline: Option<&RulePipeline>,
) -> bool {
    // The request half always comes from the window (folder, recursion,
    // order). The pipeline half prefers the pipeline recorded with the plan:
    // for ordinary previews it is exactly what the rule fields produce, and
    // for a plan rebuilt from a handoff it is the stored pipeline, which the
    // rule fields may not be able to express. Rule-field edits are still
    // caught, because any edit invalidates the stored plan immediately.
    let pipeline = stored_pipeline
        .cloned()
        .unwrap_or_else(|| pipeline_from_ui(ui));
    inputs_match(plan, &request_from_ui(ui), &pipeline)
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
    let counter_enabled = ui.get_add_counter();
    let request = request_from_ui(&ui);
    spawn_preview(
        weak,
        state,
        request,
        pipeline,
        counter_enabled,
        if use_current_rules {
            "Refreshing the non-destructive preview…"
        } else {
            "Scanning locally and building a non-destructive preview…"
        },
    );
}

/// Builds a plan from an explicit request + pipeline on a worker thread and
/// installs it as the current preview. Shared by the ordinary scan/refresh
/// path and by rebuilding a fresh preview from a handoff's stored rules.
fn spawn_preview(
    weak: slint::Weak<MainWindow>,
    state: Arc<AppState>,
    request: PlanRequest,
    pipeline: RulePipeline,
    counter_enabled: bool,
    busy_status: &str,
) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    // Building a fresh preview leaves handoff mode: the rows on screen are
    // about to be replaced by the new plan's.
    if let Ok(mut guard) = state.handoff.lock() {
        *guard = None;
    }
    ui.set_handoff_mode(false);
    ui.set_can_apply(false);
    ui.set_can_save_plan(false);
    ui.set_risk_budget("".into());
    ui.set_order_summary("".into());
    ui.set_selected_row_index(-1);
    ui.set_provenance("".into());
    ui.set_status(busy_status.into());

    thread::spawn(move || match plan(request, &pipeline) {
        Ok(new_plan) => {
            let rows = rows_for_plan(&new_plan);
            let summary = summary_text(&new_plan);
            let risk_budget = risk_budget_text(&new_plan);
            let order_summary = order_summary_text(&new_plan, counter_enabled);
            let executable = new_plan.is_executable();
            let plan_for_check = new_plan.clone();
            let pipeline_for_check = pipeline.clone();
            let suggested_path = new_plan
                .request
                .inputs
                .first()
                .map(|input| format!("{}/plan.fileweaver-plan.json", input.display()));
            if let Ok(mut current) = state.plan.lock() {
                *current = Some(new_plan);
            }
            if let Ok(mut current) = state.plan_pipeline.lock() {
                *current = Some(pipeline);
            }
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_rows(ModelRc::new(Rc::new(VecModel::from(rows))));
                // The inputs may have changed while this preview was being
                // built on the worker thread; only present the plan as
                // applicable if it still matches what the user now sees.
                // (Apply re-validates regardless, as a final guard.)
                if plan_is_current(&plan_for_check, &ui, Some(&pipeline_for_check)) {
                    ui.set_status(summary.into());
                    ui.set_risk_budget(risk_budget.into());
                    ui.set_order_summary(order_summary.into());
                    ui.set_selected_row_index(-1);
                    ui.set_provenance(
                        "Select a file row to see how its proposed name was built, rule by rule."
                            .into(),
                    );
                    ui.set_can_apply(executable);
                    ui.set_can_save_plan(executable);
                    if ui.get_plan_path().trim().is_empty()
                        && let Some(path) = &suggested_path
                    {
                        ui.set_plan_path(path.clone().into());
                    }
                } else {
                    ui.set_status(
                        "Settings changed while the preview was building. Select Refresh preview to review the updated plan before applying."
                            .into(),
                    );
                    ui.set_risk_budget("".into());
                    ui.set_order_summary("".into());
                    ui.set_selected_row_index(-1);
                    ui.set_provenance("".into());
                    ui.set_can_apply(false);
                    ui.set_can_save_plan(false);
                }
            });
        }
        Err(error) => {
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_status(format!("Could not build preview: {error}").into());
                ui.set_can_apply(false);
                ui.set_can_save_plan(false);
            });
        }
    });
}

fn schedule_apply(weak: slint::Weak<MainWindow>, state: Arc<AppState>) {
    // In handoff mode Apply means something different: re-check the saved
    // plan against the folder as it is *now* and apply only its clean rows.
    let in_handoff = state
        .handoff
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false);
    if in_handoff {
        schedule_handoff_apply(weak, state);
        return;
    }
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
    let stored_pipeline = state.plan_pipeline.lock().ok().and_then(|g| g.clone());
    if let Some(ui) = weak.upgrade()
        && !plan_is_current(&plan, &ui, stored_pipeline.as_ref())
    {
        if let Ok(mut guard) = state.plan.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = state.plan_pipeline.lock() {
            *guard = None;
        }
        ui.set_can_apply(false);
        ui.set_can_save_plan(false);
        ui.set_risk_budget("".into());
        ui.set_order_summary("".into());
        ui.set_status(
            "The folder or rules changed since this preview. Refresh the preview and review it before applying."
                .into(),
        );
        return;
    }

    run_apply(weak, state, plan, None);
}

/// Applies a handoff's still-valid rows. The drift check is re-run at click
/// time — never trust the report from when the plan was opened — and the
/// subset is rebuilt through the planner, so what executes is a fresh,
/// fully validated plan, journaled like any other job.
fn schedule_handoff_apply(weak: slint::Weak<MainWindow>, state: Arc<AppState>) {
    let file = state
        .handoff
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|view| view.file.clone()));
    let Some(file) = file else {
        return;
    };
    if let Some(ui) = weak.upgrade() {
        ui.set_can_apply(false);
        ui.set_status("Re-checking the saved plan against the folder as it is now…".into());
    }
    let weak_for_apply = weak.clone();
    let state_for_apply = Arc::clone(&state);
    thread::spawn(move || {
        let subset = (|| -> Result<Option<PlannedJob>, fileweaver_core::HandoffError> {
            let report = file.evaluate()?;
            report.applicable_plan(&file)
        })();
        match subset {
            Ok(Some(subset)) => run_apply(
                weak_for_apply,
                state_for_apply,
                subset,
                Some(" (applied from a saved plan file)"),
            ),
            Ok(None) => {
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_status(
                        "Nothing in this plan can be applied any more — every row has drifted, or the remaining rows no longer validate together. Choose Rebuild as fresh preview to start over from the plan's rules."
                            .into(),
                    );
                });
            }
            Err(error) => {
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_status(format!("Could not re-check the plan: {error}").into());
                    ui.set_can_apply(true);
                });
            }
        }
    });
}

/// Executes a fully validated plan through the journal-recording executor.
/// Shared by the ordinary apply path and the handoff path; `origin_note` is
/// appended to the completion messages for handoff-applied jobs.
fn run_apply(
    weak: slint::Weak<MainWindow>,
    state: Arc<AppState>,
    plan: PlannedJob,
    origin_note: Option<&'static str>,
) {
    let cancellation = CancellationToken::default();
    if let Ok(mut current) = state.cancellation.lock() {
        *current = Some(cancellation.clone());
    }
    // run_apply is also called from the handoff re-check worker thread, so
    // all window updates go through the event loop rather than a direct
    // upgrade.
    let _ = weak.clone().upgrade_in_event_loop(move |ui| {
        ui.set_can_apply(false);
        ui.set_can_save_plan(false);
        ui.set_applying(true);
        ui.set_status(
            "Applying reviewed changes. The app records a local recovery ledger as it works…"
                .into(),
        );
    });
    let note = origin_note.unwrap_or_default();

    let state_for_finish = Arc::clone(&state);
    thread::spawn(move || {
        let result = (|| {
            let journal = Journal::open(operation_ledger_path())?;
            let mut executor = Executor::new(journal);
            executor.execute(&plan, &cancellation, |event| update_progress(&weak, event))
        })();
        // The job has changed the folder, so the preview (and any handoff
        // view) is consumed: a fresh scan is required before anything else
        // can be applied.
        if let Ok(mut guard) = state_for_finish.plan.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = state_for_finish.plan_pipeline.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = state_for_finish.handoff.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = state_for_finish.cancellation.lock() {
            *guard = None;
        }
        refresh_ledger(weak.clone(), Arc::clone(&state_for_finish));
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_applying(false);
            ui.set_handoff_mode(false);
            ui.set_can_apply(false);
            ui.set_can_save_plan(false);
            ui.set_risk_budget("".into());
            ui.set_order_summary("".into());
            ui.set_selected_row_index(-1);
            ui.set_provenance("".into());
            match result {
            Ok(outcome) if outcome.cancelled => ui.set_status(
                format!(
                    "Stopped safely after {} changes{note}. Completed items remain recorded for recovery.",
                    outcome.applied
                )
                .into(),
            ),
            Ok(outcome) if outcome.failed > 0 => ui.set_status(
                format!(
                    "{} changes completed{note}; {} failed. Review the operation ledger before retrying.",
                    outcome.applied, outcome.failed
                )
                .into(),
            ),
            Ok(outcome) => ui.set_status(
                format!(
                    "Completed {} changes{note}. Job {} is stored locally for a future recovery review.",
                    outcome.applied, outcome.job_id
                )
                .into(),
            ),
            Err(error) => ui.set_status(format!("No changes were started: {error}").into()),
            }
        });
    });
}

/// Writes a file atomically (Contract §3 #14): contents go to a sibling
/// temporary file, are flushed to disk, and are then renamed over the
/// destination, so a crash mid-save leaves the old file or the new one —
/// never a truncated one.
fn write_file_atomically(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut temp_name = path.as_os_str().to_owned();
    temp_name.push(".tmp");
    let temp = PathBuf::from(temp_name);
    let outcome = (|| {
        fs::write(&temp, contents)?;
        fs::OpenOptions::new()
            .write(true)
            .open(&temp)
            .and_then(|file| file.sync_all())?;
        fs::rename(&temp, path)
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(&temp);
    }
    outcome
}

fn schedule_save_plan(weak: slint::Weak<MainWindow>, state: Arc<AppState>) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    let path = ui.get_plan_path().to_string();
    let path = path.trim().to_owned();
    if path.is_empty() {
        ui.set_status("Enter a plan file path first (for example, plan.fileweaver-plan.json beside the photos).".into());
        return;
    }
    let plan = state.plan.lock().ok().and_then(|guard| guard.clone());
    let pipeline = state
        .plan_pipeline
        .lock()
        .ok()
        .and_then(|guard| guard.clone());
    let (Some(plan), Some(pipeline)) = (plan, pipeline) else {
        ui.set_status("Build and review a preview before saving a plan.".into());
        return;
    };
    // Never save a plan that no longer matches the window, or one the user
    // could not apply themselves: the receiver must be able to trust that
    // this exact plan was reviewed and applicable when it was saved.
    if !plan_is_current(&plan, &ui, Some(&pipeline)) {
        ui.set_status(
            "Settings changed since the preview. Refresh the preview before saving the plan."
                .into(),
        );
        return;
    }
    if !plan.is_executable() {
        ui.set_status("Resolve blocking preview errors before saving a plan for handoff.".into());
        return;
    }
    let created_unix_ms =
        i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)
            .unwrap_or(0);
    let file = PlanFile::from_plan(&plan, &pipeline, created_unix_ms);
    let json = match file.to_json() {
        Ok(json) => json,
        Err(error) => {
            ui.set_status(format!("Could not serialize the plan: {error}").into());
            return;
        }
    };
    ui.set_status(format!("Saving plan to {path}…").into());
    thread::spawn(move || {
        let result = write_file_atomically(Path::new(&path), json.as_bytes())
            .map_err(|error| error.to_string());
        let _ = weak.upgrade_in_event_loop(move |ui| match result {
            Ok(()) => ui.set_status(
                format!(
                    "Plan saved to {path}. Send this file to the reviewer — opening it changes nothing until they choose Apply handoff. It lists file names and paths only; it is not a backup of the files."
                )
                .into(),
            ),
            Err(error) => {
                ui.set_status(format!("Could not save the plan to {path}: {error}").into())
            }
        });
    });
}

fn schedule_open_plan(weak: slint::Weak<MainWindow>, state: Arc<AppState>) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    let path = ui.get_plan_path().to_string();
    let path = path.trim().to_owned();
    if path.is_empty() {
        ui.set_status("Enter the path of a .fileweaver-plan.json file to open.".into());
        return;
    }
    ui.set_status(format!("Opening {path} and checking it against the folder…").into());
    thread::spawn(move || {
        let outcome = (|| -> Result<(PlanFile, DriftReport, Option<PlannedJob>), String> {
            let text = fs::read_to_string(&path)
                .map_err(|error| format!("Could not read {path}: {error}"))?;
            let file = PlanFile::from_json(&text).map_err(|error| error.to_string())?;
            let report = file.evaluate().map_err(|error| error.to_string())?;
            let applicable = report
                .applicable_plan(&file)
                .map_err(|error| error.to_string())?;
            Ok((file, report, applicable))
        })();
        match outcome {
            Ok((file, report, applicable)) => {
                let rows: Vec<PreviewRow> = file
                    .actions
                    .iter()
                    .zip(report.rows.iter())
                    .map(|(action, row)| PreviewRow {
                        old_name: action
                            .source
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| action.source.display().to_string())
                            .into(),
                        proposed_name: action.proposed_name.clone().into(),
                        state: drift_label(row.state).into(),
                    })
                    .collect();
                let applicable_changes = applicable
                    .as_ref()
                    .map(|subset| subset.actions.iter().filter(|a| a.will_change()).count())
                    .unwrap_or(0);
                let can_apply = applicable.is_some();
                let risk = as_proposed_text(&file);
                let order = format!(
                    "Order: {} · {} file(s) · Plan saved {}.",
                    file.request.sort.label(),
                    file.actions.len(),
                    format_unix_ms(file.created_unix_ms)
                );
                let status = drift_summary_text(&file, &report, applicable_changes);
                let folder = file
                    .request
                    .inputs
                    .first()
                    .map(|input| input.display().to_string());
                if let Ok(mut guard) = state.handoff.lock() {
                    *guard = Some(HandoffView { file });
                }
                if let Ok(mut guard) = state.plan.lock() {
                    *guard = None;
                }
                if let Ok(mut guard) = state.plan_pipeline.lock() {
                    *guard = None;
                }
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    if let Some(folder) = &folder {
                        ui.set_folder_path(folder.clone().into());
                    }
                    ui.set_handoff_mode(true);
                    ui.set_rows(ModelRc::new(Rc::new(VecModel::from(rows))));
                    ui.set_risk_budget(risk.into());
                    ui.set_order_summary(order.into());
                    ui.set_status(status.into());
                    ui.set_selected_row_index(-1);
                    ui.set_provenance(
                        "Select a file row to see how its proposed name was built, rule by rule (as proposed)."
                            .into(),
                    );
                    ui.set_can_apply(can_apply);
                    ui.set_can_save_plan(false);
                });
            }
            Err(message) => {
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_status(format!("Could not open the plan: {message}").into());
                });
            }
        }
    });
}

fn close_handoff(weak: slint::Weak<MainWindow>, state: Arc<AppState>) {
    if let Ok(mut guard) = state.handoff.lock() {
        *guard = None;
    }
    if let Some(ui) = weak.upgrade() {
        ui.set_handoff_mode(false);
        ui.set_rows(ModelRc::new(Rc::new(VecModel::from(
            Vec::<PreviewRow>::new(),
        ))));
        ui.set_can_apply(false);
        ui.set_risk_budget("".into());
        ui.set_order_summary("".into());
        ui.set_selected_row_index(-1);
        ui.set_provenance("".into());
        ui.set_status("Plan closed. Scan the folder to build a fresh preview.".into());
    }
}

fn schedule_rebuild_handoff(weak: slint::Weak<MainWindow>, state: Arc<AppState>) {
    let file = state
        .handoff
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|view| view.file.clone()));
    let Some(file) = file else {
        return;
    };
    let Some(ui) = weak.upgrade() else {
        return;
    };
    // Sync the window fields the stored request can express, so the fresh
    // preview's currency checks line up with what the reviewer sees.
    if let Some(input) = file.request.inputs.first() {
        ui.set_folder_path(input.display().to_string().into());
    }
    ui.set_recursive(file.request.recursive);
    ui.set_sort_index(sort_index_of(file.request.sort));
    let counter_enabled = file
        .pipeline
        .rules
        .iter()
        .any(|rule| matches!(rule, Rule::AppendCounter { .. }));
    let busy = if pipeline_is_shell_shape(&file.pipeline) {
        "Rebuilding a fresh preview from the plan's stored rules…"
    } else {
        "Rebuilding a fresh preview from the plan's stored rules (which include rules this window cannot edit, such as regex — the preview below is authoritative)…"
    };
    spawn_preview(
        weak,
        state,
        file.request.clone(),
        file.pipeline.clone(),
        counter_enabled,
        busy,
    );
}

fn sort_index_of(sort: SortOrder) -> i32 {
    match sort {
        SortOrder::Path => 0,
        SortOrder::Name => 1,
        SortOrder::ModifiedOldest => 2,
        SortOrder::CaptureDateOldest => 3,
    }
}

/// True when the pipeline uses only the rule shapes (and fixed counter
/// settings) the window's rule fields can produce.
fn pipeline_is_shell_shape(pipeline: &RulePipeline) -> bool {
    pipeline.rules.iter().all(|rule| match rule {
        Rule::AddPrefix { .. } => true,
        Rule::ReplaceText { replace_all, .. } => *replace_all,
        Rule::AppendCounter {
            separator,
            start_at,
            width,
        } => separator == "_" && *start_at == 1 && *width == 4,
        _ => false,
    })
}

fn drift_label(state: DriftState) -> &'static str {
    match state {
        DriftState::Unchanged => "Ready",
        DriftState::SourceChanged => "Changed since plan",
        DriftState::SourceMissing => "Source missing",
        DriftState::ResultDiffers => "Result differs now",
        DriftState::TargetNowExists => "Target now exists",
    }
}

fn as_proposed_text(file: &PlanFile) -> String {
    let summary = &file.summary;
    format!(
        "As proposed — {} to change · {} unchanged · {} blocking · {} warning(s) when this plan was saved. The drift check on the right of each row is against the folder as it exists now.",
        summary.ready, summary.unchanged, summary.blocking_errors, summary.warnings
    )
}

fn drift_summary_text(file: &PlanFile, report: &DriftReport, applicable_changes: usize) -> String {
    let new_note = if report.new_files.is_empty() {
        String::new()
    } else {
        format!(
            " {} new file(s) are not in this plan and will be left alone.",
            report.new_files.len()
        )
    };
    format!(
        "Plan opened (saved {}). Drift check: {} unchanged · {} changed · {} missing · {} result differs · {} target exists.{} {} of {} rows can be applied — drifted rows are never applied.",
        format_unix_ms(file.created_unix_ms),
        report.count(DriftState::Unchanged),
        report.count(DriftState::SourceChanged),
        report.count(DriftState::SourceMissing),
        report.count(DriftState::ResultDiffers),
        report.count(DriftState::TargetNowExists),
        new_note,
        applicable_changes,
        file.actions.len()
    )
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

/// Rule-stage provenance for one preview row: the original name followed by
/// the result of each rule stage, in pipeline order. The stages were recorded
/// by the rule engine when the plan was built — this only formats them.
fn provenance_text(action: &fileweaver_core::PlannedAction) -> String {
    provenance_for(&action.snapshot.original_name, &action.stages)
}

fn provenance_for_handoff(action: &HandoffAction) -> String {
    let original = action
        .source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| action.source.display().to_string());
    provenance_for(&original, &action.stages)
}

fn provenance_for(original_name: &str, stages: &[StageValue]) -> String {
    if stages.is_empty() {
        return format!("{original_name} — no rules in the pipeline, so the name is unchanged.");
    }
    let mut text = original_name.to_string();
    for stage in stages {
        text.push_str(&format!(" → {}: {}", stage.rule_label, stage.value));
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
    if total == 0 && review.pending.is_empty() {
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
    for item in &review.pending {
        text.push_str(&format!(
            " Pending recovery: recorded location {}; original {}; destination {} ({}).",
            item.temp.display(),
            item.source.display(),
            item.destination.display(),
            item.reason
                .as_deref()
                .unwrap_or("interrupted intent; uncertain ownership requires manual review")
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

/// The value of the last digit run in a proposed name's stem, when that run
/// is exactly four digits (the shell's fixed counter width).
fn counter_value(proposed_name: &str) -> Option<u64> {
    let stem = match proposed_name.rfind('.') {
        Some(dot) if dot > 0 => &proposed_name[..dot],
        _ => proposed_name,
    };
    let bytes = stem.as_bytes();
    let mut run: Option<(usize, usize)> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            run = Some((start, i));
        } else {
            i += 1;
        }
    }
    let (start, end) = run?;
    if end - start != 4 {
        return None;
    }
    stem[start..end].parse().ok()
}

fn order_summary_text(plan: &PlannedJob, counter_enabled: bool) -> String {
    let mut text = format!(
        "Order: {} · {} file(s) in this order.",
        plan.request.sort.label(),
        plan.summary.discovered
    );
    if counter_enabled {
        let values: Vec<u64> = plan
            .actions
            .iter()
            .filter(|action| action.will_change())
            .filter_map(|action| counter_value(&action.proposed_name))
            .collect();
        if let (Some(min), Some(max)) = (values.iter().min(), values.iter().max()) {
            text.push_str(&format!(" Assigned counters run _{min:04} to _{max:04}."));
        }
    }
    for gap in plan.sequence_gaps.iter().take(2) {
        let missing = gap
            .missing
            .iter()
            .take(5)
            .map(|value| format!("{value:0width$}", width = gap.width))
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!(
            " Possible gap in {}: missing {missing}.",
            gap.pattern
        ));
    }
    if plan.sequence_gaps.len() > 2 {
        text.push_str(&format!(
            " (+{} more patterns with gaps.)",
            plan.sequence_gaps.len() - 2
        ));
    }
    text
}

/// Where the operation ledger lives. Primary location is the portable
/// `data/operations.sqlite` beside the executable (Contract §3 #8). On first
/// run a legacy per-user ledger is copied across — the original file is
/// left untouched as a backup — and if the portable directory cannot be
/// created or written (for example the app runs from a read-only folder),
/// the legacy location is used so the app still works.
fn operation_ledger_path() -> PathBuf {
    resolve_ledger_path(env::current_exe().ok().as_deref(), &legacy_ledger_path())
}

/// The ledger location a purge targets: the file this build would use,
/// resolved *without* triggering the first-run migration copy — cleaning
/// must not create the very file it is about to delete. If no portable
/// ledger exists yet, the legacy file (if any) is the current data.
fn purge_ledger_path() -> PathBuf {
    let exe = env::current_exe().ok();
    if let Some(data_dir) = portable_data_dir(exe.as_deref()) {
        let portable = data_dir.join("operations.sqlite");
        if portable.exists() {
            return portable;
        }
    }
    let legacy = legacy_ledger_path();
    if legacy.exists() {
        return legacy;
    }
    resolve_ledger_path(exe.as_deref(), &legacy)
}

/// The pre-portable per-user ledger location, now only a migration source
/// and a fallback for unwritable install folders.
fn legacy_ledger_path() -> PathBuf {
    let base = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_STATE_HOME").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("FileWeaver").join("operations.sqlite")
}

/// The `data/` directory beside the executable, when an exe path is known.
fn portable_data_dir(exe_path: Option<&Path>) -> Option<PathBuf> {
    exe_path
        .and_then(Path::parent)
        .map(|directory| directory.join("data"))
}

fn resolve_ledger_path(exe_path: Option<&Path>, legacy: &Path) -> PathBuf {
    let Some(data_dir) = portable_data_dir(exe_path) else {
        return legacy.to_path_buf();
    };
    let portable = data_dir.join("operations.sqlite");
    if portable.exists() {
        return portable;
    }
    if fs::create_dir_all(&data_dir).is_ok() && directory_is_writable(&data_dir) {
        if legacy.exists() {
            copy_ledger_files(legacy, &portable);
        }
        return portable;
    }
    legacy.to_path_buf()
}

/// Copies a ledger and any WAL sidecars that hold not-yet-checkpointed
/// state. Best-effort by design: a failed copy means starting with a fresh
/// portable ledger, never a broken one, and the legacy file remains as the
/// backup either way.
fn copy_ledger_files(from: &Path, to: &Path) {
    let _ = fs::copy(from, to);
    for suffix in ["-wal", "-shm"] {
        let source = sidecar_path(from, suffix);
        if source.exists() {
            let _ = fs::copy(&source, sidecar_path(to, suffix));
        }
    }
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn directory_is_writable(directory: &Path) -> bool {
    let probe = directory.join(".fileweaver-write-probe");
    match fs::write(&probe, b"") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
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
            sort: SortOrder::Path,
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
    #[test]
    fn provenance_shows_each_rule_stage_in_order() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("photo.txt"), b"x").unwrap();
        let request = PlanRequest {
            inputs: vec![dir.path().to_path_buf()],
            operation: OperationKind::Rename,
            destination_root: None,
            recursive: false,
            include_directories: false,
            collision_policy: CollisionPolicy::Block,
            allow_legacy_long_paths: false,
            sort: SortOrder::Path,
        };
        let pipeline = RulePipeline {
            rules: vec![
                Rule::AddPrefix {
                    text: "2026-".into(),
                },
                Rule::ReplaceText {
                    find: "photo".into(),
                    replacement: "image".into(),
                    replace_all: true,
                },
                Rule::AppendCounter {
                    separator: "_".into(),
                    start_at: 1,
                    width: 4,
                },
            ],
        };
        let planned = plan(request, &pipeline).unwrap();
        assert_eq!(
            provenance_text(&planned.actions[0]),
            "photo.txt → Add prefix: 2026-photo.txt → Replace text: 2026-image.txt → Append counter: 2026-image_0001.txt"
        );
    }

    #[test]
    fn handoff_texts_describe_the_plan_and_its_drift() {
        let dir = tempfile::tempdir().unwrap();
        let (_request, pipeline, planned) = sample_plan(dir.path());
        let file = PlanFile::from_plan(&planned, &pipeline, 0);
        let report = file.evaluate().unwrap();
        assert!(as_proposed_text(&file).starts_with("As proposed — 1 to change"));
        let summary = drift_summary_text(&file, &report, 1);
        assert!(summary.contains("1 unchanged"));
        assert!(summary.contains("1 of 1 rows can be applied"));
        assert_eq!(
            provenance_for_handoff(&file.actions[0]),
            provenance_text(&planned.actions[0])
        );
        assert_eq!(
            drift_label(DriftState::TargetNowExists),
            "Target now exists"
        );
        assert!(pipeline_is_shell_shape(&pipeline));
    }

    #[test]
    fn non_shell_rules_are_not_shell_shape() {
        let pipeline = RulePipeline {
            rules: vec![Rule::ReplaceRegex {
                pattern: "a+".into(),
                replacement: "b".into(),
            }],
        };
        assert!(!pipeline_is_shell_shape(&pipeline));
    }

    #[test]
    fn provenance_without_rules_says_so() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("photo.txt"), b"x").unwrap();
        let request = PlanRequest {
            inputs: vec![dir.path().to_path_buf()],
            ..PlanRequest::rename(vec![])
        };
        let planned = plan(request, &RulePipeline::default()).unwrap();
        assert_eq!(
            provenance_text(&planned.actions[0]),
            "photo.txt — no rules in the pipeline, so the name is unchanged."
        );
    }

    #[test]
    fn ledger_migrates_from_legacy_location_to_portable_data_dir() {
        let exe_dir = tempfile::tempdir().unwrap();
        let legacy_dir = tempfile::tempdir().unwrap();
        let legacy = legacy_dir.path().join("operations.sqlite");
        fs::write(&legacy, b"ledger-bytes").unwrap();
        let exe = exe_dir.path().join("fileweaver.exe");
        let resolved = resolve_ledger_path(Some(&exe), &legacy);
        assert_eq!(
            resolved,
            exe_dir.path().join("data").join("operations.sqlite")
        );
        // The copy landed and the original is untouched as a backup.
        assert_eq!(fs::read(&resolved).unwrap(), b"ledger-bytes");
        assert_eq!(fs::read(&legacy).unwrap(), b"ledger-bytes");
    }

    #[test]
    fn existing_portable_ledger_wins_over_legacy() {
        let exe_dir = tempfile::tempdir().unwrap();
        let data_dir = exe_dir.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();
        let portable = data_dir.join("operations.sqlite");
        fs::write(&portable, b"portable").unwrap();
        let legacy_dir = tempfile::tempdir().unwrap();
        let legacy = legacy_dir.path().join("operations.sqlite");
        fs::write(&legacy, b"legacy").unwrap();
        let exe = exe_dir.path().join("fileweaver.exe");
        let resolved = resolve_ledger_path(Some(&exe), &legacy);
        assert_eq!(resolved, portable);
        assert_eq!(fs::read(&resolved).unwrap(), b"portable");
    }

    #[test]
    fn ledger_falls_back_to_legacy_when_data_dir_cannot_be_created() {
        let exe_dir = tempfile::tempdir().unwrap();
        // A regular file named `data` blocks creation of the data directory
        // regardless of the test user's privileges.
        fs::write(exe_dir.path().join("data"), b"not a directory").unwrap();
        let legacy_dir = tempfile::tempdir().unwrap();
        let legacy = legacy_dir.path().join("operations.sqlite");
        let exe = exe_dir.path().join("fileweaver.exe");
        assert_eq!(resolve_ledger_path(Some(&exe), &legacy), legacy);
        // No exe path at all also resolves to the legacy location.
        assert_eq!(resolve_ledger_path(None, &legacy), legacy);
    }
}
