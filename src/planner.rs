use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Error as JsonError;
use thiserror::Error;
use walkdir::WalkDir;

use crate::domain::{
    CollisionPolicy, FileFingerprint, FileSnapshot, OperationKind, PlanIssue, PlanIssueKind,
    PlanRequest, PlanSummary, PlannedAction, PlannedJob, SequenceGap, Severity, SortOrder,
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
/// Files are sorted by the request's explicit, stable sort order rather than
/// “whatever order the filesystem returns”, because that order becomes the
/// numbering order shown to users (see the Order Observatory in the shell).
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

    let sequence_gaps = detect_sequence_gaps(&actions);
    for gap in &sequence_gaps {
        global_issues.push(PlanIssue::warning(
            PlanIssueKind::SequenceGap,
            format!(
                "Possible sequence gap in {}: {} file(s) match {}, but {} number(s) in the range are missing (first missing: {}). A gap can be legitimate — for example a deleted photo — so nothing is blocked.",
                gap.folder.display(),
                gap.present,
                gap.pattern,
                gap.missing_count,
                gap.missing
                    .first()
                    .map(|value| format!("{value:0width$}", width = gap.width))
                    .unwrap_or_default(),
            ),
        ));
    }

    let summary = summarize(&actions, &global_issues);
    Ok(PlannedJob {
        request,
        pipeline_json: serde_json::to_string_pretty(pipeline)?,
        global_issues,
        actions,
        summary,
        sequence_gaps,
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
    sort_snapshots(&mut snapshots, request.sort);
    (snapshots, issues)
}

/// Applies the plan's numbering order. Every comparison ends with the stable
/// path key, so the result is fully deterministic even when the chosen key
/// ties (same name in different folders, identical timestamps).
fn sort_snapshots(snapshots: &mut [FileSnapshot], order: SortOrder) {
    let path_cmp = |a: &FileSnapshot, b: &FileSnapshot| {
        stable_path_key(&a.source).cmp(&stable_path_key(&b.source))
    };
    match order {
        SortOrder::Path => snapshots.sort_by(path_cmp),
        SortOrder::Name => snapshots.sort_by(|a, b| {
            a.original_name
                .to_lowercase()
                .cmp(&b.original_name.to_lowercase())
                .then_with(|| path_cmp(a, b))
        }),
        SortOrder::ModifiedOldest => snapshots.sort_by(|a, b| {
            // Files without a modification time sort last.
            let key = |s: &FileSnapshot| (s.modified_unix_nanos.is_none(), s.modified_unix_nanos);
            key(a).cmp(&key(b)).then_with(|| path_cmp(a, b))
        }),
        SortOrder::CaptureDateOldest => snapshots.sort_by(|a, b| {
            // The stored capture string is kamadak-exif's fixed-width
            // "YYYY-MM-DD HH:MM:SS" display form, so lexicographic order is
            // chronological. Files without capture time sort last.
            let key =
                |s: &FileSnapshot| (s.jpeg_exif_datetime.is_none(), s.jpeg_exif_datetime.clone());
            key(a).cmp(&key(b)).then_with(|| path_cmp(a, b))
        }),
    }
}

/// Detects holes in numbered filename sequences over the proposed names.
///
/// Files are grouped by folder and by their proposed name with the *last*
/// run of digits replaced by a marker. A group needs at least three members
/// before a hole in its numeric range is reported, which keeps one-off names
/// (a lone `notes2.txt`) from producing noise.
struct SequenceGroup {
    pattern: String,
    width: usize,
    numbers: Vec<u64>,
}

fn detect_sequence_gaps(actions: &[PlannedAction]) -> Vec<SequenceGap> {
    use std::collections::BTreeMap;
    // Keyed by (folder, masked stem, extension).
    let mut groups: BTreeMap<(PathBuf, String, String), SequenceGroup> = BTreeMap::new();
    for action in actions {
        let name = &action.proposed_name;
        let (stem, extension) = match name.rfind('.') {
            Some(dot) if dot > 0 => (&name[..dot], &name[dot + 1..]),
            _ => (name.as_str(), ""),
        };
        let bytes = stem.as_bytes();
        // Find the last maximal run of ASCII digits in the stem.
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
        let Some((start, end)) = run else {
            continue;
        };
        let Ok(value) = stem[start..end].parse::<u64>() else {
            continue;
        };
        let width = end - start;
        // Group key: literal stem text around a single marker. Display
        // pattern: the same text with one `#` per digit of the varying run.
        let masked = format!("{}#{}", &stem[..start], &stem[end..]);
        let mut pattern = format!("{}{}{}", &stem[..start], "#".repeat(width), &stem[end..]);
        if !extension.is_empty() {
            pattern.push_str(&format!(".{extension}"));
        }
        let folder = action
            .destination
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let group = groups
            .entry((folder, masked, extension.to_string()))
            .or_insert_with(|| SequenceGroup {
                pattern,
                width,
                numbers: Vec::new(),
            });
        group.numbers.push(value);
    }

    let mut gaps = Vec::new();
    for ((folder, _masked, _extension), group) in groups {
        let SequenceGroup {
            pattern,
            width,
            mut numbers,
        } = group;
        if numbers.len() < 3 {
            continue;
        }
        numbers.sort_unstable();
        numbers.dedup();
        let (min, max) = (numbers[0], numbers[numbers.len() - 1]);
        let span = max - min + 1;
        let present = u64::try_from(numbers.len()).unwrap_or(u64::MAX);
        if span <= present {
            continue;
        }
        let present_set: std::collections::BTreeSet<u64> = numbers.iter().copied().collect();
        let missing: Vec<u64> = (min..=max)
            .filter(|value| !present_set.contains(value))
            .take(20)
            .collect();
        gaps.push(SequenceGap {
            pattern,
            width,
            folder,
            present: numbers.len(),
            missing_count: usize::try_from(span - present).unwrap_or(usize::MAX),
            missing,
        });
    }
    gaps
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
    fn fabricated_snapshot(
        path: &str,
        modified: Option<i128>,
        capture: Option<&str>,
    ) -> FileSnapshot {
        FileSnapshot {
            source: PathBuf::from(path),
            original_name: path.rsplit('/').next().unwrap_or(path).to_string(),
            is_directory: false,
            fingerprint: FileFingerprint {
                size_bytes: 1,
                modified_unix_nanos: modified,
            },
            modified_unix_nanos: modified,
            jpeg_exif_datetime: capture.map(str::to_string),
        }
    }

    #[test]
    fn name_sort_differs_from_path_sort() {
        let mut snapshots = vec![
            fabricated_snapshot("/root/zz/a.txt", None, None),
            fabricated_snapshot("/root/aa/z.txt", None, None),
        ];
        sort_snapshots(&mut snapshots, SortOrder::Path);
        assert_eq!(snapshots[0].original_name, "z.txt");
        sort_snapshots(&mut snapshots, SortOrder::Name);
        assert_eq!(snapshots[0].original_name, "a.txt");
    }

    #[test]
    fn modified_sort_puts_oldest_first_and_unknown_last() {
        let mut snapshots = vec![
            fabricated_snapshot("/root/new.txt", Some(300), None),
            fabricated_snapshot("/root/unknown.txt", None, None),
            fabricated_snapshot("/root/old.txt", Some(100), None),
        ];
        sort_snapshots(&mut snapshots, SortOrder::ModifiedOldest);
        let order: Vec<&str> = snapshots
            .iter()
            .map(|snapshot| snapshot.original_name.as_str())
            .collect();
        assert_eq!(order, ["old.txt", "new.txt", "unknown.txt"]);
    }

    #[test]
    fn capture_sort_puts_oldest_first_and_unknown_last() {
        let mut snapshots = vec![
            fabricated_snapshot("/root/late.jpg", None, Some("2026-06-01 10:00:00")),
            fabricated_snapshot("/root/none.jpg", None, None),
            fabricated_snapshot("/root/early.jpg", None, Some("2024-01-15 09:30:00")),
        ];
        sort_snapshots(&mut snapshots, SortOrder::CaptureDateOldest);
        let order: Vec<&str> = snapshots
            .iter()
            .map(|snapshot| snapshot.original_name.as_str())
            .collect();
        assert_eq!(order, ["early.jpg", "late.jpg", "none.jpg"]);
    }

    #[test]
    fn counter_numbering_follows_the_chosen_sort() {
        let directory = tempdir().unwrap();
        fs::create_dir(directory.path().join("aa")).unwrap();
        fs::create_dir(directory.path().join("zz")).unwrap();
        fs::write(directory.path().join("aa/z.txt"), "z").unwrap();
        fs::write(directory.path().join("zz/a.txt"), "a").unwrap();
        let pipeline = RulePipeline {
            rules: vec![Rule::AppendCounter {
                separator: "_".into(),
                start_at: 1,
                width: 4,
            }],
        };
        let proposed_for = |planned: &PlannedJob, original: &str| {
            planned
                .actions
                .iter()
                .find(|action| action.snapshot.original_name == original)
                .unwrap()
                .proposed_name
                .clone()
        };

        let mut by_path = PlanRequest::rename(vec![directory.path().to_path_buf()]);
        by_path.recursive = true;
        let planned = plan(by_path, &pipeline).unwrap();
        assert_eq!(proposed_for(&planned, "z.txt"), "z_0001.txt");
        assert_eq!(proposed_for(&planned, "a.txt"), "a_0002.txt");

        let mut by_name = PlanRequest::rename(vec![directory.path().to_path_buf()]);
        by_name.recursive = true;
        by_name.sort = SortOrder::Name;
        let planned = plan(by_name, &pipeline).unwrap();
        assert_eq!(proposed_for(&planned, "a.txt"), "a_0001.txt");
        assert_eq!(proposed_for(&planned, "z.txt"), "z_0002.txt");
    }

    #[test]
    fn sequence_gap_is_detected_in_proposed_names() {
        let directory = tempdir().unwrap();
        for name in [
            "IMG_0001.jpg",
            "IMG_0002.jpg",
            "IMG_0004.jpg",
            "IMG_0007.jpg",
        ] {
            fs::write(directory.path().join(name), "x").unwrap();
        }
        let planned = plan(
            PlanRequest::rename(vec![directory.path().to_path_buf()]),
            &RulePipeline::default(),
        )
        .unwrap();
        assert_eq!(planned.sequence_gaps.len(), 1);
        let gap = &planned.sequence_gaps[0];
        assert_eq!(gap.pattern, "IMG_####.jpg");
        assert_eq!(gap.present, 4);
        assert_eq!(gap.missing, vec![3, 5, 6]);
        assert!(
            planned
                .global_issues
                .iter()
                .any(|issue| issue.kind == PlanIssueKind::SequenceGap
                    && issue.severity == Severity::Warning)
        );
    }

    #[test]
    fn complete_sequence_has_no_gap() {
        let directory = tempdir().unwrap();
        for name in ["IMG_0001.jpg", "IMG_0002.jpg", "IMG_0003.jpg"] {
            fs::write(directory.path().join(name), "x").unwrap();
        }
        // A lone pair with a hole is below the reporting threshold.
        fs::write(directory.path().join("clip_01.mp4"), "x").unwrap();
        fs::write(directory.path().join("clip_03.mp4"), "x").unwrap();
        let planned = plan(
            PlanRequest::rename(vec![directory.path().to_path_buf()]),
            &RulePipeline::default(),
        )
        .unwrap();
        assert!(planned.sequence_gaps.is_empty());
    }
}
