#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::BTreeSet;
use std::env;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;

use fileweaver_core::{
    CancellationToken, CaseStyle, CollisionPolicy, DateSource, Divergence, ExecuteError,
    ExecuteProgress, Executor, JobReview, JobSummary, Journal, JournalError, OperationKind,
    PlanRequest, PlannedJob, PresetListing, PresetStore, PresetSummary, RollbackReport, Rule,
    RulePipeline, SortOrder, plan, subset_plan,
};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

slint::include_modules!();

#[derive(Default)]
struct AppState {
    plan: Mutex<Option<PlannedJob>>,
    /// Source paths the user unchecked in the preview. Keyed the same way the
    /// planner keys collisions, so the choice survives a re-render and is
    /// applied through `subset_plan` rather than by editing the plan.
    excluded: Mutex<BTreeSet<String>>,
    /// The ordered rule stack shown in the shell. This is the single source of
    /// truth for the next preview; the UI only edits it.
    rules: Mutex<Vec<Rule>>,
    selected_rule: Mutex<Option<usize>>,
    /// Presets as last listed from disk, in the order the combo box shows.
    presets: Mutex<Vec<PresetSummary>>,
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

    ui.on_select_row({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move |index| {
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            let text = state.plan.lock().ok().and_then(|guard| {
                guard
                    .as_ref()
                    .and_then(|plan| plan.actions.get(index))
                    .map(provenance_text)
            });
            if let (Some(ui), Some(text)) = (weak.upgrade(), text) {
                ui.set_selected_row_index(index as i32);
                ui.set_provenance(text.into());
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

    ui.on_add_rule({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move |kind| {
            let rule = default_rule_for_kind(kind);
            let index = match state.rules.lock() {
                Ok(mut rules) => {
                    rules.push(rule);
                    rules.len() - 1
                }
                Err(_) => return,
            };
            if let Ok(mut selected) = state.selected_rule.lock() {
                *selected = Some(index);
            }
            if let Some(ui) = weak.upgrade() {
                refresh_rule_ui(&ui, &state);
                clear_preview(&state, &ui);
            }
        }
    });

    ui.on_remove_rule({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move || {
            let Some(index) = selected_rule(&state) else {
                return;
            };
            let mut next_selection = None;
            {
                let Ok(mut rules) = state.rules.lock() else {
                    return;
                };
                if index >= rules.len() {
                    return;
                }
                rules.remove(index);
                if !rules.is_empty() {
                    next_selection = Some(index.min(rules.len() - 1));
                }
            }
            if let Ok(mut selected) = state.selected_rule.lock() {
                *selected = next_selection;
            }
            if let Some(ui) = weak.upgrade() {
                refresh_rule_ui(&ui, &state);
                clear_preview(&state, &ui);
            }
        }
    });

    ui.on_move_rule_up({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move || {
            let Some(index) = selected_rule(&state) else {
                return;
            };
            if index == 0 {
                return;
            }
            match state.rules.lock() {
                Ok(mut rules) if index < rules.len() => rules.swap(index - 1, index),
                _ => return,
            }
            if let Ok(mut selected) = state.selected_rule.lock() {
                *selected = Some(index - 1);
            }
            if let Some(ui) = weak.upgrade() {
                refresh_rule_rows(&ui, &state);
                clear_preview(&state, &ui);
            }
        }
    });

    ui.on_move_rule_down({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move || {
            let Some(index) = selected_rule(&state) else {
                return;
            };
            match state.rules.lock() {
                Ok(mut rules) if index + 1 < rules.len() => rules.swap(index, index + 1),
                _ => return,
            }
            if let Ok(mut selected) = state.selected_rule.lock() {
                *selected = Some(index + 1);
            }
            if let Some(ui) = weak.upgrade() {
                refresh_rule_rows(&ui, &state);
                clear_preview(&state, &ui);
            }
        }
    });

    ui.on_select_rule({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move |index| {
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            match state.rules.lock() {
                Ok(rules) if index < rules.len() => {}
                _ => return,
            }
            if let Ok(mut selected) = state.selected_rule.lock() {
                *selected = Some(index);
            }
            if let Some(ui) = weak.upgrade() {
                refresh_rule_ui(&ui, &state);
            }
        }
    });

    ui.on_toggle_row({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move |index| {
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let key = state.plan.lock().ok().and_then(|guard| {
                guard
                    .as_ref()
                    .and_then(|plan| plan.actions.get(index))
                    .map(|action| plan_key(&action.snapshot.source))
            });
            let Some(key) = key else {
                return;
            };
            if let Ok(mut excluded) = state.excluded.lock()
                && !excluded.remove(&key)
            {
                excluded.insert(key);
            }
            refresh_preview_view(&ui, &state);
        }
    });

    ui.on_editor_changed({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(index) = selected_rule(&state) else {
                return;
            };
            let updated = {
                let Ok(mut rules) = state.rules.lock() else {
                    return;
                };
                let Some(rule) = rules.get_mut(index) else {
                    return;
                };
                apply_editor_to_rule(&ui, rule);
                rule.clone()
            };
            ui.set_editor_hint(editor_hint(&updated).into());
            refresh_rule_rows(&ui, &state);
            clear_preview(&state, &ui);
        }
    });

    ui.on_save_preset({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let name = ui.get_preset_name().trim().to_owned();
            if name.is_empty() {
                ui.set_status("Enter a preset name before saving.".into());
                return;
            }
            let pipeline = current_pipeline(&state);
            let store = preset_store();
            let state_for_thread = Arc::clone(&state);
            let weak_for_thread = weak.clone();
            thread::spawn(move || {
                let result = store
                    .save(&name, &pipeline)
                    .and_then(|summary| Ok((summary, store.list()?)));
                let _ = weak_for_thread.upgrade_in_event_loop(move |ui| match result {
                    Ok((summary, listing)) => {
                        apply_preset_listing(&ui, &state_for_thread, &listing);
                        ui.set_status(
                            format!(
                                "Saved preset \u{201c}{}\u{201d} ({} rule(s)). The edited rule stack is unchanged.",
                                summary.name, summary.rule_count
                            )
                            .into(),
                        );
                    }
                    Err(error) => {
                        ui.set_status(format!("Could not save the preset: {error}").into());
                    }
                });
            });
        }
    });

    ui.on_load_preset({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(summary) = selected_preset(&state, &ui) else {
                ui.set_status("Select a saved preset first.".into());
                return;
            };
            ui.set_confirming_preset_delete(false);
            let store = preset_store();
            let state_for_thread = Arc::clone(&state);
            let weak_for_thread = weak.clone();
            thread::spawn(move || {
                let result = store.load(&summary.file_name);
                let _ = weak_for_thread.upgrade_in_event_loop(move |ui| match result {
                    Ok(file) => {
                        let name = file.name.clone();
                        let count = file.pipeline.rules.len();
                        if let Ok(mut rules) = state_for_thread.rules.lock() {
                            *rules = file.pipeline.rules;
                        }
                        if let Ok(mut selected) = state_for_thread.selected_rule.lock() {
                            *selected = (count > 0).then_some(0);
                        }
                        refresh_rule_ui(&ui, &state_for_thread);
                        clear_preview(&state_for_thread, &ui);
                        ui.set_status(
                            format!(
                                "Loaded preset \u{201c}{name}\u{201d} ({count} rule(s)). Select Refresh preview to review the updated plan before applying."
                            )
                            .into(),
                        );
                    }
                    Err(error) => {
                        ui.set_status(
                            format!("Could not load preset \u{201c}{}\u{201d}: {error}", summary.name)
                                .into(),
                        );
                    }
                });
            });
        }
    });

    ui.on_delete_preset({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(summary) = selected_preset(&state, &ui) else {
                ui.set_status("Select a saved preset first.".into());
                return;
            };
            if !ui.get_confirming_preset_delete() {
                ui.set_confirming_preset_delete(true);
                ui.set_status(
                    format!(
                        "Confirm deleting the preset \u{201c}{}\u{201d}. This removes the saved rule stack only; the current rule stack is unchanged. Select Delete again to continue.",
                        summary.name
                    )
                    .into(),
                );
                return;
            }
            ui.set_confirming_preset_delete(false);
            let store = preset_store();
            let state_for_thread = Arc::clone(&state);
            let weak_for_thread = weak.clone();
            thread::spawn(move || {
                let result = store.delete(&summary.file_name).and_then(|()| store.list());
                let _ = weak_for_thread.upgrade_in_event_loop(move |ui| match result {
                    Ok(listing) => {
                        apply_preset_listing(&ui, &state_for_thread, &listing);
                        ui.set_status(
                            format!("Deleted the preset \u{201c}{}\u{201d}.", summary.name).into(),
                        );
                    }
                    Err(error) => {
                        ui.set_status(
                            format!(
                                "Could not delete the preset \u{201c}{}\u{201d}: {error}",
                                summary.name
                            )
                            .into(),
                        );
                    }
                });
            });
        }
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

    // Preset listing at startup is a small, bounded folder read, like the
    // initial history load above; it runs before the event loop so the combo
    // box is populated on first paint.
    match preset_store().list() {
        Ok(listing) => apply_preset_listing(&ui, &state, &listing),
        Err(error) => {
            ui.set_preset_detail(format!("Could not read saved presets: {error}").into());
        }
    }

    ui.on_inputs_changed({
        let state = Arc::clone(&state);
        let weak = ui.as_weak();
        move || {
            // Any edit to the folder or numbering order invalidates the stored
            // preview: the plan it produced must never be applied as if it
            // were current.
            if let Some(ui) = weak.upgrade() {
                clear_preview(&state, &ui);
            }
        }
    });

    refresh_rule_ui(&ui, &state);

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

fn plan_is_current(plan: &PlannedJob, ui: &MainWindow, state: &AppState) -> bool {
    inputs_match(plan, &request_from_ui(ui), &current_pipeline(state))
}

/// Snapshot of the ordered rule stack, which is the pipeline the next preview
/// will use. A poisoned lock yields an empty stack rather than a panic: the
/// preview then cannot match any stored plan, which fails safe.
fn current_pipeline(state: &AppState) -> RulePipeline {
    RulePipeline {
        rules: current_rules(state),
    }
}

fn selected_rule(state: &AppState) -> Option<usize> {
    state.selected_rule.lock().ok().and_then(|guard| *guard)
}

/// Discards the stored preview after any input change, so a plan built from
/// settings the user no longer sees can never be applied.
fn clear_preview(state: &AppState, ui: &MainWindow) {
    let had_plan = match state.plan.lock() {
        Ok(mut guard) => guard.take().is_some(),
        Err(_) => false,
    };
    if had_plan {
        ui.set_can_apply(false);
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

    let pipeline = current_pipeline(&state);
    let request = request_from_ui(&ui);
    // A fresh scan starts from every file included; the previous exclusion
    // choices belong to a plan that is being replaced.
    if let Ok(mut excluded) = state.excluded.lock() {
        excluded.clear();
    }
    ui.set_can_apply(false);
    ui.set_risk_budget("".into());
    ui.set_order_summary("".into());
    ui.set_selected_row_index(-1);
    ui.set_provenance("".into());
    ui.set_status(
        if use_current_rules {
            "Refreshing the non-destructive preview…"
        } else {
            "Scanning locally and building a non-destructive preview…"
        }
        .into(),
    );

    // The ordering summary needs the pipeline's counter settings; keep a copy
    // because the pipeline itself is borrowed by the planning call.
    let summary_pipeline = pipeline.clone();
    thread::spawn(move || match plan(request, &pipeline) {
        Ok(new_plan) => {
            let rows = rows_for_plan(&new_plan, &BTreeSet::new());
            let summary = summary_text(&new_plan);
            let risk_budget = risk_budget_text(&new_plan, 0);
            let order_summary = order_summary_text(&new_plan, &summary_pipeline);
            let executable = new_plan.is_executable();
            let plan_for_check = new_plan.clone();
            let state_for_check = Arc::clone(&state);
            if let Ok(mut current) = state.plan.lock() {
                *current = Some(new_plan);
            }
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_rows(ModelRc::new(Rc::new(VecModel::from(rows))));
                // The inputs may have changed while this preview was being
                // built on the worker thread; only present the plan as
                // applicable if it still matches what the user now sees.
                // (Apply re-validates regardless, as a final guard.)
                if plan_is_current(&plan_for_check, &ui, &state_for_check) {
                    ui.set_status(summary.into());
                    ui.set_risk_budget(risk_budget.into());
                    ui.set_order_summary(order_summary.into());
                    ui.set_selected_row_index(-1);
                    ui.set_provenance(
                        "Select a file row to see how its proposed name was built, rule by rule."
                            .into(),
                    );
                    ui.set_can_apply(executable);
                } else {
                    // The result no longer matches the visible settings, so it
                    // must not remain available to Apply either.
                    if let Ok(mut stored) = state_for_check.plan.lock() {
                        *stored = None;
                    }
                    if let Ok(mut excluded) = state_for_check.excluded.lock() {
                        excluded.clear();
                    }
                    ui.set_status(
                        "Settings changed while the preview was building. Select Refresh preview to review the updated plan before applying."
                            .into(),
                    );
                    ui.set_risk_budget("".into());
                    ui.set_order_summary("".into());
                    ui.set_selected_row_index(-1);
                    ui.set_provenance("".into());
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
    // Final guard: never execute a plan whose folder or rules no longer match
    // what the user sees, even if an input change somehow reached Apply
    // without invalidating the preview first.
    if let Some(ui) = weak.upgrade()
        && !plan_is_current(&plan, &ui, &state)
    {
        if let Ok(mut guard) = state.plan.lock() {
            *guard = None;
        }
        ui.set_can_apply(false);
        ui.set_risk_budget("".into());
        ui.set_order_summary("".into());
        ui.set_status(
            "The folder or rules changed since this preview. Refresh the preview and review it before applying."
                .into(),
        );
        return;
    }

    // Only the rows the user still has included are applied. `subset_plan`
    // re-runs collision and filename validation for the subset, so an
    // exclusion can never bypass a blocking error.
    let excluded_count = state.excluded.lock().map(|set| set.len()).unwrap_or(0);
    let applicable = applicable_plan_of(&plan, &excluded_set(&state));
    if !applicable.is_executable() {
        if let Some(ui) = weak.upgrade() {
            ui.set_status(
                if applicable.summary.ready == 0 {
                    "No files are included for apply. Include at least one row, or refresh the preview."
                } else {
                    "Resolve blocking preview errors before applying changes."
                }
                .into(),
            );
        }
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
            executor.execute(&applicable, &cancellation, |event| {
                update_progress(&weak, event)
            })
        })();
        // The job has changed the folder, so the preview is consumed: a fresh
        // scan is required before anything else can be applied.
        if let Ok(mut guard) = state_for_finish.plan.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = state_for_finish.excluded.lock() {
            guard.clear();
        }
        if let Ok(mut guard) = state_for_finish.cancellation.lock() {
            *guard = None;
        }
        refresh_ledger(weak.clone(), Arc::clone(&state_for_finish));
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_applying(false);
            ui.set_can_apply(false);
            ui.set_risk_budget("".into());
            ui.set_order_summary("".into());
            ui.set_selected_row_index(-1);
            ui.set_provenance("".into());
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
                    "Completed {} changes{}. Job {} is stored locally for a future recovery review.",
                    outcome.applied,
                    if excluded_count > 0 {
                        format!("; {excluded_count} excluded row(s) were left untouched")
                    } else {
                        String::new()
                    },
                    outcome.job_id
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

fn current_rules(state: &AppState) -> Vec<Rule> {
    state
        .rules
        .lock()
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

/// The rule kind indices are a UI contract: they index the "Rule type to add"
/// combo box in `ui/main.slint`. Keep both in the same order.
fn default_rule_for_kind(kind: i32) -> Rule {
    match kind {
        1 => Rule::AddSuffixBeforeExtension {
            text: String::new(),
        },
        2 => Rule::ReplaceText {
            find: String::new(),
            replacement: String::new(),
            replace_all: true,
        },
        3 => Rule::ReplaceRegex {
            pattern: String::new(),
            replacement: String::new(),
        },
        4 => Rule::ChangeCase {
            style: CaseStyle::Lower,
        },
        5 => Rule::TrimAndCollapseWhitespace,
        6 => Rule::ChangeExtension {
            extension: String::new(),
        },
        7 => Rule::PrefixDate {
            source: DateSource::ModifiedTime,
            format: "[year]-[month]-[day]".into(),
            separator: "_".into(),
        },
        8 => Rule::AppendCounter {
            separator: "_".into(),
            start_at: 1,
            width: 4,
        },
        _ => Rule::AddPrefix {
            text: String::new(),
        },
    }
}

fn rule_kind_index(rule: &Rule) -> i32 {
    match rule {
        Rule::AddPrefix { .. } => 0,
        Rule::AddSuffixBeforeExtension { .. } => 1,
        Rule::ReplaceText { .. } => 2,
        Rule::ReplaceRegex { .. } => 3,
        Rule::ChangeCase { .. } => 4,
        Rule::TrimAndCollapseWhitespace => 5,
        Rule::ChangeExtension { .. } => 6,
        Rule::PrefixDate { .. } => 7,
        Rule::AppendCounter { .. } => 8,
    }
}

fn case_style_from_index(index: i32) -> CaseStyle {
    match index {
        1 => CaseStyle::Upper,
        2 => CaseStyle::Title,
        _ => CaseStyle::Lower,
    }
}

fn case_style_to_index(style: CaseStyle) -> i32 {
    match style {
        CaseStyle::Lower => 0,
        CaseStyle::Upper => 1,
        CaseStyle::Title => 2,
    }
}

fn date_source_from_index(index: i32) -> DateSource {
    match index {
        1 => DateSource::JpegCaptureTime,
        _ => DateSource::ModifiedTime,
    }
}

fn date_source_to_index(source: DateSource) -> i32 {
    match source {
        DateSource::ModifiedTime => 0,
        DateSource::JpegCaptureTime => 1,
    }
}

fn date_source_label(source: DateSource) -> &'static str {
    match source {
        DateSource::ModifiedTime => "modified time",
        DateSource::JpegCaptureTime => "JPEG capture date",
    }
}

fn rule_summary(rule: &Rule) -> String {
    match rule {
        Rule::AddPrefix { text } => format!("adds “{text}” at the start"),
        Rule::AddSuffixBeforeExtension { text } => {
            format!("adds “{text}” before the extension")
        }
        Rule::ReplaceText {
            find,
            replacement,
            replace_all,
        } => format!(
            "“{find}” → “{replacement}” ({})",
            if *replace_all {
                "every match"
            } else {
                "first match"
            }
        ),
        Rule::ReplaceRegex {
            pattern,
            replacement,
        } => format!("regex “{pattern}” → “{replacement}”"),
        Rule::ChangeCase { style } => format!("whole name to {style}"),
        Rule::TrimAndCollapseWhitespace => "trims and collapses spaces".to_owned(),
        Rule::ChangeExtension { extension } => {
            let cleaned = extension.trim().trim_start_matches('.');
            if cleaned.is_empty() {
                "removes the extension".to_owned()
            } else {
                format!("extension → .{cleaned}")
            }
        }
        Rule::PrefixDate {
            source,
            format,
            separator,
        } => format!(
            "{format} from {} then “{separator}”",
            date_source_label(*source)
        ),
        Rule::AppendCounter {
            separator,
            start_at,
            width,
        } => format!("“{separator}” + numbers from {start_at}, {width} digits"),
    }
}

/// Short, rule-specific note under the editor fields. These carry information
/// the controls themselves cannot: what the rule needs, and what blocks Apply.
fn editor_hint(rule: &Rule) -> String {
    match rule {
        Rule::ReplaceText { .. } => {
            "Literal text, not a pattern. Use Replace regex for patterns.".to_owned()
        }
        Rule::ReplaceRegex { .. } => {
            "Rust regular expression syntax. An invalid pattern blocks Apply and is reported in the preview."
                .to_owned()
        }
        Rule::ChangeExtension { .. } => {
            "The final extension is replaced. A leading dot is optional; a blank value removes the extension."
                .to_owned()
        }
        Rule::PrefixDate {
            source: DateSource::JpegCaptureTime,
            ..
        } => "Needs a JPEG Exif capture date. JPEGs without one are blocked with an explanation; other file types never have one."
            .to_owned(),
        Rule::PrefixDate { .. } => {
            "Uses each file’s filesystem modified time. Dates are formatted in UTC so the names are reproducible."
                .to_owned()
        }
        Rule::AppendCounter { .. } => {
            "Numbers are assigned in the numbering order chosen below, across every file in the plan."
                .to_owned()
        }
        Rule::ChangeCase { .. } => {
            "Applies to the whole name, including the extension.".to_owned()
        }
        Rule::AddPrefix { .. } => "Blank text changes nothing.".to_owned(),
        Rule::AddSuffixBeforeExtension { .. } => {
            "Inserted before the final extension; a blank value changes nothing.".to_owned()
        }
        Rule::TrimAndCollapseWhitespace => String::new(),
    }
}

fn rule_rows(rules: &[Rule]) -> Vec<RuleRow> {
    rules
        .iter()
        .enumerate()
        .map(|(index, rule)| RuleRow {
            position: format!("{}.", index + 1).into(),
            label: rule.label().into(),
            summary: rule_summary(rule).into(),
        })
        .collect()
}

/// Rebuilds the rule list display and clamps the selection to it. It never
/// rewrites the editor fields, so typing is not interrupted.
fn refresh_rule_rows(ui: &MainWindow, state: &AppState) {
    let rules = current_rules(state);
    ui.set_rule_rows(ModelRc::new(Rc::new(VecModel::from(rule_rows(&rules)))));
    let selected = selected_rule(state).filter(|index| *index < rules.len());
    if let Ok(mut guard) = state.selected_rule.lock() {
        *guard = selected;
    }
    ui.set_selected_rule_index(match selected {
        Some(index) => i32::try_from(index).unwrap_or(-1),
        None => -1,
    });
}

/// Refreshes the rule list and loads the selected rule into the editor.
fn refresh_rule_ui(ui: &MainWindow, state: &AppState) {
    refresh_rule_rows(ui, state);
    let rule = selected_rule(state).and_then(|index| current_rules(state).get(index).cloned());
    match rule {
        Some(rule) => push_rule_to_editor(ui, &rule),
        None => {
            ui.set_selected_rule_index(-1);
            ui.set_editor_kind(-1);
            ui.set_editor_hint("".into());
        }
    }
}

fn push_rule_to_editor(ui: &MainWindow, rule: &Rule) {
    ui.set_editor_kind(rule_kind_index(rule));
    match rule {
        Rule::AddPrefix { text } | Rule::AddSuffixBeforeExtension { text } => {
            ui.set_editor_text(text.clone().into());
        }
        Rule::ReplaceText {
            find,
            replacement,
            replace_all,
        } => {
            ui.set_editor_find(find.clone().into());
            ui.set_editor_replacement(replacement.clone().into());
            ui.set_editor_replace_all(*replace_all);
        }
        Rule::ReplaceRegex {
            pattern,
            replacement,
        } => {
            ui.set_editor_find(pattern.clone().into());
            ui.set_editor_replacement(replacement.clone().into());
        }
        Rule::ChangeCase { style } => ui.set_editor_case_index(case_style_to_index(*style)),
        Rule::TrimAndCollapseWhitespace => {}
        Rule::ChangeExtension { extension } => {
            ui.set_editor_extension(extension.clone().into());
        }
        Rule::PrefixDate {
            source,
            format,
            separator,
        } => {
            ui.set_editor_date_source_index(date_source_to_index(*source));
            ui.set_editor_date_format(format.clone().into());
            ui.set_editor_date_separator(separator.clone().into());
        }
        Rule::AppendCounter {
            separator,
            start_at,
            width,
        } => {
            ui.set_editor_counter_separator(separator.clone().into());
            ui.set_editor_counter_start(i32::try_from(*start_at).unwrap_or(1));
            ui.set_editor_counter_width(i32::from(*width));
        }
    }
    ui.set_editor_hint(editor_hint(rule).into());
}

/// Copies every editor field into the selected rule. Fields that do not belong
/// to the selected rule kind are ignored, because the shell only shows the
/// controls that apply.
fn apply_editor_to_rule(ui: &MainWindow, rule: &mut Rule) {
    match rule {
        Rule::AddPrefix { text } | Rule::AddSuffixBeforeExtension { text } => {
            *text = ui.get_editor_text().to_string();
        }
        Rule::ReplaceText {
            find,
            replacement,
            replace_all,
        } => {
            *find = ui.get_editor_find().to_string();
            *replacement = ui.get_editor_replacement().to_string();
            *replace_all = ui.get_editor_replace_all();
        }
        Rule::ReplaceRegex {
            pattern,
            replacement,
        } => {
            *pattern = ui.get_editor_find().to_string();
            *replacement = ui.get_editor_replacement().to_string();
        }
        Rule::ChangeCase { style } => *style = case_style_from_index(ui.get_editor_case_index()),
        Rule::TrimAndCollapseWhitespace => {}
        Rule::ChangeExtension { extension } => {
            *extension = ui.get_editor_extension().to_string();
        }
        Rule::PrefixDate {
            source,
            format,
            separator,
        } => {
            *source = date_source_from_index(ui.get_editor_date_source_index());
            *format = ui.get_editor_date_format().to_string();
            *separator = ui.get_editor_date_separator().to_string();
        }
        Rule::AppendCounter {
            separator,
            start_at,
            width,
        } => {
            *separator = ui.get_editor_counter_separator().to_string();
            *start_at = u64::try_from(ui.get_editor_counter_start())
                .unwrap_or(1)
                .max(1);
            *width = u8::try_from(ui.get_editor_counter_width())
                .unwrap_or(4)
                .clamp(1, 9);
        }
    }
}

fn rows_for_plan(plan: &PlannedJob, excluded: &BTreeSet<String>) -> Vec<PreviewRow> {
    plan.actions
        .iter()
        .map(|action| {
            let included = !excluded.contains(&plan_key(&action.snapshot.source));
            PreviewRow {
                old_name: action.snapshot.original_name.clone().into(),
                proposed_name: action.proposed_name.clone().into(),
                state: row_state(action, included).into(),
                included,
            }
        })
        .collect()
}

fn row_state(action: &fileweaver_core::PlannedAction, included: bool) -> String {
    if !included {
        return "Excluded — will not change".into();
    }
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

fn risk_budget_text(plan: &PlannedJob, excluded: usize) -> String {
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
    if excluded > 0 {
        text.push_str(&format!(
            " {excluded} row(s) excluded by you and will not change."
        ));
    }
    text
}

/// Case-insensitive path key, matching the planner's collision key, so an
/// exclusion follows the file even if the proposal is recomputed.
fn plan_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

fn excluded_set(state: &AppState) -> BTreeSet<String> {
    state
        .excluded
        .lock()
        .map(|set| set.clone())
        .unwrap_or_default()
}

/// The plan Apply would execute: the full plan with the user's exclusions
/// removed, revalidated as a subset by the planner.
fn applicable_plan_of(plan: &PlannedJob, excluded: &BTreeSet<String>) -> PlannedJob {
    if excluded.is_empty() {
        return plan.clone();
    }
    subset_plan(plan, |action| {
        !excluded.contains(&plan_key(&action.snapshot.source))
    })
}

fn preview_status_text(full: &PlannedJob, applicable: &PlannedJob, excluded: usize) -> String {
    if excluded == 0 {
        return summary_text(full);
    }
    format!(
        "{} {excluded} row(s) excluded — Apply will change {} of {} discovered file(s).",
        summary_text(full),
        applicable.summary.ready,
        full.summary.discovered
    )
}

/// Re-renders the preview from the stored plan after an inclusion change and
/// recomputes every consequence: the rows, the risk budget, the order summary,
/// and whether Apply is available.
fn refresh_preview_view(ui: &MainWindow, state: &AppState) {
    let Some(plan) = state.plan.lock().ok().and_then(|guard| guard.clone()) else {
        return;
    };
    let excluded = excluded_set(state);
    let applicable = applicable_plan_of(&plan, &excluded);
    let rows = rows_for_plan(&plan, &excluded);
    ui.set_rows(ModelRc::new(Rc::new(VecModel::from(rows))));
    ui.set_selected_row_index(-1);
    ui.set_provenance("".into());
    ui.set_risk_budget(risk_budget_text(&applicable, excluded.len()).into());
    ui.set_order_summary(order_summary_text(&applicable, &current_pipeline(state)).into());
    ui.set_can_apply(applicable.is_executable());
    ui.set_status(preview_status_text(&plan, &applicable, excluded.len()).into());
}

/// Rule-stage provenance for one preview row: the original name followed by
/// the result of each rule stage, in pipeline order. The stages were recorded
/// by the rule engine when the plan was built — this only formats them.
fn provenance_text(action: &fileweaver_core::PlannedAction) -> String {
    if action.stages.is_empty() {
        return format!(
            "{} — no rules in the pipeline, so the name is unchanged.",
            action.snapshot.original_name
        );
    }
    let mut text = action.snapshot.original_name.clone();
    for stage in &action.stages {
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

/// The value of the last digit run in a proposed name's stem, when that run
/// is exactly `width` digits — the width the counter rule was configured with.
fn counter_value(proposed_name: &str, width: usize) -> Option<u64> {
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
    if end - start != width {
        return None;
    }
    stem[start..end].parse().ok()
}

fn order_summary_text(plan: &PlannedJob, pipeline: &RulePipeline) -> String {
    let mut text = format!(
        "Order: {} · {} file(s) in this order.",
        plan.request.sort.label(),
        plan.summary.discovered
    );
    let counter = pipeline.rules.iter().find_map(|rule| match rule {
        Rule::AppendCounter {
            separator, width, ..
        } => Some((separator.as_str(), usize::from(*width))),
        _ => None,
    });
    if let Some((separator, width)) = counter {
        let values: Vec<u64> = plan
            .actions
            .iter()
            .filter(|action| action.will_change())
            .filter_map(|action| counter_value(&action.proposed_name, width))
            .collect();
        if let (Some(min), Some(max)) = (values.iter().min(), values.iter().max()) {
            let formatted = |value: u64| format!("{:0width$}", value, width = width);
            text.push_str(&format!(
                " Assigned counters run {separator}{} to {separator}{}.",
                formatted(*min),
                formatted(*max)
            ));
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

/// Local application-state folder. Resolution order (documented in the
/// README): `LOCALAPPDATA`, then `XDG_STATE_HOME`, then `~/.local/state`, then
/// the working directory.
fn state_directory() -> PathBuf {
    let base = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_STATE_HOME").map(PathBuf::from))
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("FileWeaver")
}

fn operation_ledger_path() -> PathBuf {
    state_directory().join("operations.sqlite")
}

fn preset_store() -> PresetStore {
    PresetStore::new(state_directory().join("presets"))
}

/// The preset the combo box currently points at, if any.
fn selected_preset(state: &AppState, ui: &MainWindow) -> Option<PresetSummary> {
    let index = usize::try_from(ui.get_preset_index()).ok()?;
    state.presets.lock().ok()?.get(index).cloned()
}

/// Publishes a listing to both the shell model and the state the handlers
/// resolve preset indices against. The selection is cleared because the list
/// contents (and therefore the indices) may have changed.
fn apply_preset_listing(ui: &MainWindow, state: &AppState, listing: &PresetListing) {
    if let Ok(mut presets) = state.presets.lock() {
        *presets = listing.presets.clone();
    }
    let names: Vec<SharedString> = listing
        .presets
        .iter()
        .map(|preset| SharedString::from(preset.name.as_str()))
        .collect();
    ui.set_preset_names(ModelRc::new(Rc::new(VecModel::from(names))));
    ui.set_preset_index(-1);
    ui.set_confirming_preset_delete(false);
    ui.set_preset_detail(preset_detail_text(listing).into());
}

fn preset_detail_text(listing: &PresetListing) -> String {
    let warnings = listing
        .warnings
        .iter()
        .take(2)
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    if listing.presets.is_empty() {
        if warnings.is_empty() {
            "No saved presets yet. Name the current rule stack below and select Save preset to reuse it later."
                .to_owned()
        } else {
            format!("No usable presets were found. {warnings}")
        }
    } else {
        let base = format!(
            "{} saved preset(s). Loading a preset replaces the current rule stack; nothing is applied to files.",
            listing.presets.len()
        );
        if warnings.is_empty() {
            base
        } else {
            format!("{base} {warnings}")
        }
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
    fn every_rule_kind_round_trips_through_the_shell_index() {
        for kind in 0..9 {
            let rule = default_rule_for_kind(kind);
            assert_eq!(rule_kind_index(&rule), kind);
            assert!(!rule_summary(&rule).is_empty());
        }
    }

    #[test]
    fn case_and_date_choices_round_trip() {
        for index in 0..3 {
            assert_eq!(case_style_to_index(case_style_from_index(index)), index);
        }
        for index in 0..2 {
            assert_eq!(date_source_to_index(date_source_from_index(index)), index);
        }
    }

    #[test]
    fn rule_stack_is_the_pipeline_used_for_the_next_preview() {
        let state = AppState::default();
        assert!(current_pipeline(&state).rules.is_empty());
        if let Ok(mut rules) = state.rules.lock() {
            rules.push(default_rule_for_kind(5));
        }
        let pipeline = current_pipeline(&state);
        assert_eq!(pipeline.rules.len(), 1);
        assert!(matches!(
            pipeline.rules.first(),
            Some(Rule::TrimAndCollapseWhitespace)
        ));
    }

    #[test]
    fn counter_summary_reads_the_configured_width() {
        assert_eq!(counter_value("photo_0007.jpg", 4), Some(7));
        assert_eq!(counter_value("photo_07.jpg", 4), None);
        assert_eq!(counter_value("photo_07.jpg", 2), Some(7));
    }

    #[test]
    fn excluded_rows_leave_the_applicable_plan_and_are_marked() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        fs::write(dir.path().join("b.txt"), "b").unwrap();
        let pipeline = RulePipeline {
            rules: vec![Rule::AddPrefix { text: "x-".into() }],
        };
        let planned = plan(
            PlanRequest::rename(vec![dir.path().to_path_buf()]),
            &pipeline,
        )
        .unwrap();
        assert_eq!(planned.actions.len(), 2);

        let mut excluded = BTreeSet::new();
        excluded.insert(plan_key(&planned.actions[0].snapshot.source));

        let applicable = applicable_plan_of(&planned, &excluded);
        assert_eq!(applicable.actions.len(), 1);
        assert_eq!(applicable.actions[0].snapshot.original_name, "b.txt");
        assert!(applicable.is_executable());

        let rows = rows_for_plan(&planned, &excluded);
        assert_eq!(rows.len(), 2);
        assert!(!rows[0].included);
        assert!(rows[0].state.contains("Excluded"));
        assert!(rows[1].included);
        assert!(preview_status_text(&planned, &applicable, 1).contains("1 row(s) excluded"));
        assert!(risk_budget_text(&applicable, 1).contains("excluded by you"));

        // Excluding everything leaves nothing to apply.
        excluded.insert(plan_key(&planned.actions[1].snapshot.source));
        let nothing = applicable_plan_of(&planned, &excluded);
        assert_eq!(nothing.summary.ready, 0);
        assert!(!nothing.is_executable());
    }

    #[test]
    fn preset_detail_text_reports_counts_and_warnings() {
        let listing = PresetListing {
            presets: vec![PresetSummary {
                name: "Photo cleanup".into(),
                file_name: "photo-cleanup.json".into(),
                rule_count: 2,
            }],
            warnings: vec!["Skipped broken.json: not a preset".into()],
        };
        let text = preset_detail_text(&listing);
        assert!(text.contains("1 saved preset(s)"), "{text}");
        assert!(text.contains("Skipped broken.json"), "{text}");

        let empty = preset_detail_text(&PresetListing::default());
        assert!(empty.contains("No saved presets yet"), "{empty}");
    }

    #[test]
    fn counter_summary_states_the_assigned_range() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["one.txt", "two.txt"] {
            fs::write(dir.path().join(name), b"x").unwrap();
        }
        let pipeline = RulePipeline {
            rules: vec![Rule::AppendCounter {
                separator: "-".into(),
                start_at: 3,
                width: 2,
            }],
        };
        let planned = plan(
            PlanRequest::rename(vec![dir.path().to_path_buf()]),
            &pipeline,
        )
        .unwrap();
        let summary = order_summary_text(&planned, &pipeline);
        assert!(
            summary.contains("Assigned counters run -03 to -04"),
            "{summary}"
        );
    }
}
