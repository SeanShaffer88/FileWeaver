//! Command-line maintenance surface (Contract §2/§3 #10, #19, #26).
//!
//! These commands run before the GUI exists and exit with a status code:
//! 0 success, 1 declined/refused/failed, 2 bad usage. Opening the GUI is
//! unaffected: no arguments — or a first argument that is not a flag, such
//! as a folder path — starts the GUI exactly as before.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::win32;

pub const USAGE: &str = "\
FileWeaver — local-first batch renaming and file organization

USAGE:
    fileweaver                       Start the desktop app
    fileweaver --help                Show this help
    fileweaver --version             Show the version
    fileweaver --clean [options]     Delete FileWeaver's local data
    fileweaver --uninstall [options]
                                     Same data deletion as --clean; the
                                     program files themselves are removed
                                     by the installer / Windows Settings

OPTIONS (with --clean / --uninstall):
    --dry-run    List what would be removed; remove nothing
    --silent     Do not ask for confirmation
    --json       Print the result as JSON on stdout

EXIT CODES:
    0  success
    1  declined, refused, or failed
    2  bad usage

--clean and --uninstall refuse to run while another FileWeaver instance
is open. They remove only the data at the location this build currently
uses: the operation ledger (and its sidecar files) in the data folder
beside the executable, or at the per-user fallback location when that
folder is not writable. A backup ledger left at the old per-user
location by the one-time portable migration is NOT removed — and will
be copied back on the next launch; delete that file manually to erase
FileWeaver's data completely.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliCommand {
    Help,
    Version,
    Clean {
        dry_run: bool,
        silent: bool,
        json: bool,
    },
    Uninstall {
        dry_run: bool,
        silent: bool,
        json: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseOutcome {
    /// Start the GUI.
    Gui,
    Command(CliCommand),
    UsageError(String),
}

/// Pure argument parsing: no I/O, no process state.
pub fn parse_args(args: &[String]) -> ParseOutcome {
    if args.is_empty() {
        return ParseOutcome::Gui;
    }
    if !args[0].starts_with('-') {
        return ParseOutcome::Gui;
    }
    let mut command: Option<&str> = None;
    let (mut dry_run, mut silent, mut json) = (false, false, false);
    for arg in args {
        match arg.as_str() {
            "--help" | "--version" | "--clean" | "--uninstall" => {
                if let Some(previous) = command
                    && previous != arg
                {
                    return ParseOutcome::UsageError(format!(
                        "choose one command ({previous} and {arg} were both given)"
                    ));
                }
                command = Some(arg.as_str());
            }
            "--dry-run" => dry_run = true,
            "--silent" => silent = true,
            "--json" => json = true,
            other if other.starts_with('-') => {
                return ParseOutcome::UsageError(format!("unknown option {other}"));
            }
            other => {
                return ParseOutcome::UsageError(format!("unexpected argument {other}"));
            }
        }
    }
    match command {
        Some("--help") => ParseOutcome::Command(CliCommand::Help),
        Some("--version") => ParseOutcome::Command(CliCommand::Version),
        Some("--clean") => ParseOutcome::Command(CliCommand::Clean {
            dry_run,
            silent,
            json,
        }),
        Some("--uninstall") => ParseOutcome::Command(CliCommand::Uninstall {
            dry_run,
            silent,
            json,
        }),
        _ => ParseOutcome::UsageError(
            "--dry-run, --silent and --json only modify --clean or --uninstall".into(),
        ),
    }
}

/// The exact set of paths a purge targets, in removal order: the ledger,
/// its SQLite sidecar files, then the containing folder — which is only
/// actually removed if it ends up empty, so a folder shared with anything
/// else is never taken.
pub fn plan_purge(ledger_path: &Path) -> Vec<PathBuf> {
    let mut targets: Vec<PathBuf> = [
        ledger_path.to_path_buf(),
        sidecar_path(ledger_path, "-wal"),
        sidecar_path(ledger_path, "-shm"),
    ]
    .into_iter()
    .filter(|path| path.exists())
    .collect();
    if let Some(parent) = ledger_path.parent()
        && !parent.as_os_str().is_empty()
    {
        targets.push(parent.to_path_buf());
    }
    targets
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Runs a parsed command and returns the process exit code.
pub fn run(command: CliCommand, ledger_path: &Path) -> i32 {
    match command {
        CliCommand::Help => {
            println!("{USAGE}");
            0
        }
        CliCommand::Version => {
            println!("FileWeaver {}", env!("CARGO_PKG_VERSION"));
            0
        }
        CliCommand::Clean {
            dry_run,
            silent,
            json,
        } => purge("clean", dry_run, silent, json, ledger_path),
        CliCommand::Uninstall {
            dry_run,
            silent,
            json,
        } => purge("uninstall", dry_run, silent, json, ledger_path),
    }
}

fn purge(action: &str, dry_run: bool, silent: bool, json: bool, ledger_path: &Path) -> i32 {
    // Deleting the ledger under a running instance would fight that
    // instance's writes, so maintenance holds the single-instance mutex
    // for the duration of the purge.
    let Some(_guard) = win32::acquire_single_instance() else {
        return report_error(
            json,
            action,
            "another FileWeaver instance is running; close it before removing FileWeaver's data",
        );
    };

    let targets = plan_purge(ledger_path);
    if dry_run {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "action": action,
                    "dry_run": true,
                    "targets": path_strings(&targets),
                })
            );
        } else if targets.is_empty() {
            println!("Nothing to remove: no FileWeaver data was found.");
        } else {
            println!("Would remove:");
            print_targets(&targets);
        }
        return 0;
    }

    if targets.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::json!({"action": action, "dry_run": false, "removed": [], "ok": true})
            );
        } else {
            println!("Nothing to remove: no FileWeaver data was found.");
        }
        return finish_uninstall(action, json, 0);
    }

    if !silent {
        let prompt = format!("Remove {} FileWeaver data path(s)? [y/N] ", targets.len());
        if json {
            // The prompt stays off stdout so the JSON result stays clean.
            eprint!("{prompt}");
            let _ = std::io::stderr().flush();
        } else {
            println!("The following will be removed:");
            print_targets(&targets);
            print!("{prompt}");
            let _ = std::io::stdout().flush();
        }
        if !read_confirmation() {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"action": action, "ok": false, "error": "declined"})
                );
            } else {
                println!("Aborted; nothing was removed.");
            }
            return 1;
        }
    }

    let mut removed: Vec<PathBuf> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for target in &targets {
        let is_directory = target.is_dir();
        let result = if is_directory {
            std::fs::remove_dir(target)
        } else {
            std::fs::remove_file(target)
        };
        match result {
            Ok(()) => removed.push(target.clone()),
            Err(error) if is_directory && error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                // Other content shares the folder; leaving it is correct,
                // not a failure.
            }
            Err(error) => failures.push(format!("{}: {error}", target.display())),
        }
    }

    if !failures.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "action": action,
                    "ok": false,
                    "removed": path_strings(&removed),
                    "error": failures.join("; "),
                })
            );
        } else {
            for failure in &failures {
                eprintln!("Could not remove {failure}");
            }
        }
        return 1;
    }

    if json {
        let mut result = serde_json::json!({
            "action": action,
            "dry_run": false,
            "removed": path_strings(&removed),
            "ok": true,
        });
        if action == "uninstall" {
            result["note"] = serde_json::Value::String(UNINSTALL_NOTE.into());
        }
        println!("{result}");
    } else {
        for path in &removed {
            println!("Removed {}", path.display());
        }
    }
    finish_uninstall(action, json, 0)
}

const UNINSTALL_NOTE: &str = "To remove the program files, uninstall FileWeaver from Windows Settings (Apps) or run the uninstaller in the install folder.";

fn finish_uninstall(action: &str, json: bool, code: i32) -> i32 {
    if action == "uninstall" && !json && code == 0 {
        println!("{UNINSTALL_NOTE}");
    }
    code
}

fn report_error(json: bool, action: &str, message: &str) -> i32 {
    if json {
        println!(
            "{}",
            serde_json::json!({"action": action, "ok": false, "error": message})
        );
    } else {
        eprintln!("fileweaver: {message}");
    }
    1
}

fn print_targets(targets: &[PathBuf]) {
    for target in targets {
        if target.is_dir() {
            println!("  {} (folder, if empty)", target.display());
        } else {
            println!("  {}", target.display());
        }
    }
}

fn path_strings(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

fn read_confirmation() -> bool {
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(_) => matches!(line.trim(), "y" | "Y"),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn no_args_and_plain_paths_start_the_gui() {
        assert_eq!(parse_args(&[]), ParseOutcome::Gui);
        assert_eq!(parse_args(&args(&["C:\\photos"])), ParseOutcome::Gui);
    }

    #[test]
    fn parses_each_command_and_modifier() {
        assert_eq!(
            parse_args(&args(&["--help"])),
            ParseOutcome::Command(CliCommand::Help)
        );
        assert_eq!(
            parse_args(&args(&["--version"])),
            ParseOutcome::Command(CliCommand::Version)
        );
        assert_eq!(
            parse_args(&args(&["--clean"])),
            ParseOutcome::Command(CliCommand::Clean {
                dry_run: false,
                silent: false,
                json: false
            })
        );
        assert_eq!(
            parse_args(&args(&["--uninstall", "--dry-run", "--silent", "--json"])),
            ParseOutcome::Command(CliCommand::Uninstall {
                dry_run: true,
                silent: true,
                json: true
            })
        );
        // Modifier order does not matter, and repeats are harmless.
        assert_eq!(
            parse_args(&args(&["--json", "--clean", "--json"])),
            ParseOutcome::Command(CliCommand::Clean {
                dry_run: false,
                silent: false,
                json: true
            })
        );
    }

    #[test]
    fn rejects_bad_usage() {
        assert!(matches!(
            parse_args(&args(&["--frobnicate"])),
            ParseOutcome::UsageError(_)
        ));
        assert!(matches!(
            parse_args(&args(&["--clean", "--help"])),
            ParseOutcome::UsageError(_)
        ));
        assert!(matches!(
            parse_args(&args(&["--json"])),
            ParseOutcome::UsageError(_)
        ));
        assert!(matches!(
            parse_args(&args(&["--clean", "extra"])),
            ParseOutcome::UsageError(_)
        ));
    }

    #[test]
    fn purge_plan_lists_ledger_sidecars_then_folder() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data");
        fs::create_dir_all(&data).unwrap();
        let ledger = data.join("operations.sqlite");
        fs::write(&ledger, b"db").unwrap();
        fs::write(sidecar_path(&ledger, "-wal"), b"wal").unwrap();
        // No -shm file exists, so it must not be planned.
        let plan = plan_purge(&ledger);
        assert_eq!(
            plan,
            vec![ledger.clone(), sidecar_path(&ledger, "-wal"), data.clone()]
        );
    }

    #[test]
    fn purge_plan_for_missing_ledger_is_just_the_folder() {
        let directory = tempfile::tempdir().unwrap();
        let ledger = directory.path().join("data").join("operations.sqlite");
        assert_eq!(plan_purge(&ledger), vec![directory.path().join("data")]);
    }

    #[test]
    fn purge_dry_run_removes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data");
        fs::create_dir_all(&data).unwrap();
        let ledger = data.join("operations.sqlite");
        fs::write(&ledger, b"db").unwrap();
        let code = run(
            CliCommand::Clean {
                dry_run: true,
                silent: true,
                json: true,
            },
            &ledger,
        );
        assert_eq!(code, 0);
        assert!(ledger.exists());
    }

    #[test]
    fn purge_silent_run_removes_ledger_and_empty_folder() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data");
        fs::create_dir_all(&data).unwrap();
        let ledger = data.join("operations.sqlite");
        fs::write(&ledger, b"db").unwrap();
        fs::write(sidecar_path(&ledger, "-wal"), b"wal").unwrap();
        let code = run(
            CliCommand::Clean {
                dry_run: false,
                silent: true,
                json: false,
            },
            &ledger,
        );
        assert_eq!(code, 0);
        assert!(!ledger.exists());
        assert!(!data.exists());
    }
}
