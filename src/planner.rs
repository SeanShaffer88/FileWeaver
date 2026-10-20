use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Error as JsonError;
use thiserror::Error;
use walkdir::WalkDir;

use crate::domain::{
    CollisionPolicy, FileFingerprint, FileSnapshot, OperationKind, PlanIssue, PlanIssueKind,
    PlanRequest, PlanSummary, PlannedAction, PlannedJob, Severity,
};
use crate::metadata;
use crate::rules::{RulePipeline, RuleProblem};

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("the rule pipeline could not be serialized: {0}")]
    Serialization(#[from] JsonError),
}

/// Produces a plan without changing the filesystem.
///
/// Files are sorted by an explicit, stable case-insensitive path key. This is
/// intentionally simple rather than “whatever order the filesystem returns”,
/// because the key becomes the numbering order shown to users.
pub fn plan(request: PlanRequest, pipeline: &RulePipeline) -> Result<PlannedJob, PlanError> {
    let mut global_issues = Vec::new();
    let pipeline_is_valid = match pipeline.validate() {
        Ok(()) => true,
        Err(problem) => {
            global_issues.push(rule_issue(problem));
            false
        }
    };

    if matches!(request.operation, OperationKind::Copy | OperationKind::Move)
        && request.destination_root.is_none()
    {
        global_issues.push(PlanIssue::error(
            PlanIssueKind::MissingDestinationRoot,
            "Copy and move plans need a destination folder.",
        ));
    }

    let (snapshots, scan_issues) = collect_snapshots(&request);
    global_issues.extend(scan_issues);

    let mut actions = Vec::with_capacity(snapshots.len());
    for (index, snapshot) in snapshots.into_iter().enumerate() {
        let (proposed_name, stages, mut issues) = if pipeline_is_valid {
            match pipeline.apply(&snapshot, index) {
                Ok((name, stages)) => (name, stages, Vec::new()),
                Err(problem) => (
                    snapshot.original_name.clone(),
                    Vec::new(),
                    vec![rule_issue(problem)],
                ),
            }
        } else {
            (
                snapshot.original_name.clone(),
                Vec::new(),
                vec![PlanIssue::error(
                    PlanIssueKind::InvalidRule,
                    "The rule pipeline has an error; no proposal was generated.",
                )],
            )
        };

        issues.extend(validate_filename_component(&proposed_name));
        let destination = destination_for(&request, &snapshot.source, &proposed_name);
        if !request.allow_legacy_long_paths && legacy_path_length(&destination) > 259 {
            issues.push(PlanIssue::error(
                PlanIssueKind::LegacyPathTooLong,
                format!(
                    "The proposed path is {} UTF-16 code units long. It exceeds the conservative 259-character limit.",
                    legacy_path_length(&destination)
                ),
            ));
        }
        if snapshot.source == destination {
            issues.push(PlanIssue::info(
                PlanIssueKind::NoChange,
                "The rule produces the current name and location.",
            ));
        }

        actions.push(PlannedAction {
            index,
            snapshot,
            destination,
            proposed_name,
            stages,
            issues,
        });
    }

    apply_collision_validation(&mut actions, request.collision_policy);

    let summary = summarize(&actions, &global_issues);
    Ok(PlannedJob {
        request,
        pipeline_json: serde_json::to_string_pretty(pipeline)?,
        global_issues,
        actions,
        summary,
    })
}

fn collect_snapshots(request: &PlanRequest) -> (Vec<FileSnapshot>, Vec<PlanIssue>) {
    let mut paths = BTreeSet::new();
    let mut issues = Vec::new();

    for input in &request.inputs {
        let metadata = match fs::symlink_metadata(input) {
            Ok(metadata) => metadata,
            Err(error) => {
                issues.push(PlanIssue::error(
                    input_error_kind(&error),
                    format!("Could not inspect {}: {error}", input.display()),
                ));
                continue;
            }
        };

        if metadata.file_type().is_symlink() {
            issues.push(PlanIssue::warning(
                PlanIssueKind::InputUnavailable,
                format!(
                    "Skipped symbolic link {}. Following links is deliberately not enabled in the initial release.",
                    input.display()
                ),
            ));
            continue;
        }

        if metadata.is_dir() {
            let max_depth = if request.recursive { usize::MAX } else { 1 };
            for entry in WalkDir::new(input)
                .follow_links(false)
                .max_depth(max_depth)
                .into_iter()
            {
                match entry {
                    Ok(entry) if entry.depth() == 0 => {
                        if request.include_directories {
                            paths.insert(entry.path().to_path_buf());
                        }
                    }
                    Ok(entry) => {
                        let entry_type = entry.file_type();
                        if entry_type.is_symlink() {
                            issues.push(PlanIssue::warning(
                                PlanIssueKind::InputUnavailable,
                                format!("Skipped symbolic link {}.", entry.path().display()),
                            ));
                        } else if entry_type.is_file()
                            || (request.include_directories && entry_type.is_dir())
                        {
                            paths.insert(entry.path().to_path_buf());
                        }
                    }
                    Err(error) => issues.push(PlanIssue::error(
                        PlanIssueKind::InputUnavailable,
                        format!("Could not scan {}: {error}", input.display()),
                    )),
                }
            }
        } else if metadata.is_file() {
            paths.insert(input.clone());
        } else {
            issues.push(PlanIssue::warning(
                PlanIssueKind::InputUnavailable,
                format!("Skipped unsupported filesystem item {}.", input.display()),
            ));
        }
    }

    let mut snapshots = Vec::new();
    for path in paths {
        match snapshot(&path) {
            Ok(snapshot) => snapshots.push(snapshot),
            Err(issue) => issues.push(issue),
        }
    }
    snapshots.sort_by_key(|snapshot| stable_path_key(&snapshot.source));
    (snapshots, issues)
}

fn snapshot(path: &Path) -> Result<FileSnapshot, PlanIssue> {
    let metadata = fs::metadata(path).map_err(|error| {
        PlanIssue::error(
            input_error_kind(&error),
            format!("Could not read {}: {error}", path.display()),
        )
    })?;
    let original_name = path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .ok_or_else(|| {
            PlanIssue::error(
                PlanIssueKind::InputUnavailable,
                format!(
                    "{} does not have a usable final path component.",
                    path.display()
                ),
            )
        })?;
    let fingerprint = FileFingerprint::from_metadata(&metadata);
    Ok(FileSnapshot {
        source: path.to_path_buf(),
        original_name,
        is_directory: metadata.is_dir(),
        modified_unix_nanos: fingerprint.modified_unix_nanos,
        fingerprint,
        jpeg_exif_datetime: if metadata.is_file() {
            metadata::jpeg_capture_datetime(path)
        } else {
            None
        },
    })
}

fn destination_for(request: &PlanRequest, source: &Path, proposed_name: &str) -> PathBuf {
    match &request.destination_root {
        Some(root) => root.join(proposed_name),
        None => source
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(proposed_name),
    }
}

fn apply_collision_validation(actions: &mut [PlannedAction], policy: CollisionPolicy) {
    if !matches!(policy, CollisionPolicy::Block) {
        return;
    }

    let source_keys: BTreeSet<String> = actions
        .iter()
        .map(|action| windows_path_key(&action.snapshot.source))
        .collect();
    let mut target_groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (position, action) in actions.iter().enumerate() {
        target_groups
            .entry(windows_path_key(&action.destination))
            .or_default()
            .push(position);
    }

    for (target, positions) in target_groups {
        if positions.len() > 1 {
            for position in positions {
                actions[position].issues.push(PlanIssue::error(
                    PlanIssueKind::DuplicateTarget,
                    format!("More than one selected item proposes {target}."),
                ));
            }
        }
    }

    for action in actions {
        let target_key = windows_path_key(&action.destination);
        let own_source_key = windows_path_key(&action.snapshot.source);
        if target_key != own_source_key
            && !source_keys.contains(&target_key)
            && action.destination.exists()
        {
            action.issues.push(PlanIssue::error(
                PlanIssueKind::ExistingTarget,
                format!(
                    "{} already exists and FileWeaver will not overwrite it.",
                    action.destination.display()
                ),
            ));
        }
    }
}

fn summarize(actions: &[PlannedAction], global_issues: &[PlanIssue]) -> PlanSummary {
    let all_issues = actions
        .iter()
        .flat_map(|action| action.issues.iter())
        .chain(global_issues.iter());
    let (warnings, blocking_errors) =
        all_issues.fold((0, 0), |(warnings, errors), issue| match issue.severity {
            Severity::Warning => (warnings + 1, errors),
            Severity::Error => (warnings, errors + 1),
            Severity::Info => (warnings, errors),
        });
    let ready = actions
        .iter()
        .filter(|action| action.will_change() && !action.is_blocked())
        .count();
    let unchanged = actions
        .iter()
        .filter(|action| !action.will_change())
        .count();
    let cross_volume_candidates = actions
        .iter()
        .filter(|action| volume_key(&action.snapshot.source) != volume_key(&action.destination))
        .count();
    PlanSummary {
        discovered: actions.len(),
        ready,
        unchanged,
        warnings,
        blocking_errors,
        cross_volume_candidates,
    }
}

fn validate_filename_component(name: &str) -> Vec<PlanIssue> {
    let mut issues = Vec::new();
    if name.is_empty() {
        issues.push(PlanIssue::error(
            PlanIssueKind::EmptyName,
            "A filename cannot be empty.",
        ));
        return issues;
    }
    if name.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            )
    }) {
        issues.push(PlanIssue::error(
            PlanIssueKind::InvalidCharacter,
            "Windows filenames cannot contain control characters or < > : \" / \\ | ? *.",
        ));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        issues.push(PlanIssue::error(
            PlanIssueKind::TrailingDotOrSpace,
            "Windows filenames cannot end with a period or space.",
        ));
    }
    if name.encode_utf16().count() > 255 {
        issues.push(PlanIssue::error(
            PlanIssueKind::ComponentTooLong,
            "A Windows filename component cannot exceed 255 UTF-16 code units.",
        ));
    }
    let base = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&base.as_str()) {
        issues.push(PlanIssue::error(
            PlanIssueKind::ReservedWindowsName,
            format!("{base} is a reserved Windows device name."),
        ));
    }
    issues
}

fn rule_issue(problem: RuleProblem) -> PlanIssue {
    match problem {
        RuleProblem::MissingCaptureTime | RuleProblem::MissingModifiedTime => {
            PlanIssue::error(PlanIssueKind::MissingMetadata, problem.to_string())
        }
        _ => PlanIssue::error(PlanIssueKind::InvalidRule, problem.to_string()),
    }
}

fn input_error_kind(error: &std::io::Error) -> PlanIssueKind {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        PlanIssueKind::PermissionDenied
    } else {
        PlanIssueKind::InputUnavailable
    }
}

fn legacy_path_length(path: &Path) -> usize {
    path.as_os_str().to_string_lossy().encode_utf16().count()
}

fn stable_path_key(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

/// A deliberately conservative approximation of normal Windows case-insensitive
/// comparison. Full NTFS collation can differ for locale-specific Unicode; the
/// Windows integration test suite must cover that before release.
fn windows_path_key(path: &Path) -> String {
    path.to_string_lossy().to_uppercase()
}

fn volume_key(path: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        use std::path::Component;
        match path.components().next() {
            Some(Component::Prefix(prefix)) => {
                Some(prefix.as_os_str().to_string_lossy().to_uppercase())
            }
            _ => None,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::Rule;
    use tempfile::tempdir;

    #[test]
    fn duplicate_targets_are_blocking() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "a").unwrap();
        fs::write(directory.path().join("b.txt"), "b").unwrap();
        let pipeline = RulePipeline {
            rules: vec![Rule::ReplaceRegex {
                pattern: ".+".into(),
                replacement: "same.txt".into(),
            }],
        };
        let plan = plan(
            PlanRequest::rename(vec![directory.path().to_path_buf()]),
            &pipeline,
        )
        .unwrap();
        assert_eq!(plan.summary.blocking_errors, 2);
        assert!(!plan.is_executable());
    }

    #[test]
    fn windows_reserved_name_is_rejected() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("input.txt"), "input").unwrap();
        let pipeline = RulePipeline {
            rules: vec![Rule::ReplaceRegex {
                pattern: ".+".into(),
                replacement: "CON.txt".into(),
            }],
        };
        let plan = plan(
            PlanRequest::rename(vec![directory.path().join("input.txt")]),
            &pipeline,
        )
        .unwrap();
        assert!(
            plan.actions[0]
                .issues
                .iter()
                .any(|issue| issue.kind == PlanIssueKind::ReservedWindowsName)
        );
    }

    #[test]
    fn non_recursive_folder_scan_only_includes_direct_files() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("first.txt"), "a").unwrap();
        fs::create_dir(directory.path().join("child")).unwrap();
        fs::write(directory.path().join("child/second.txt"), "b").unwrap();
        let plan = plan(
            PlanRequest::rename(vec![directory.path().to_path_buf()]),
            &RulePipeline::default(),
        )
        .unwrap();
        assert_eq!(plan.summary.discovered, 1);
    }
}
