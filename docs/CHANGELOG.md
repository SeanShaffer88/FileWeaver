# Changelog

All notable changes to FileWeaver are recorded here.

## 0.2.0 — Unreleased

- **Portable data directory.** The operation ledger moved to `data\operations.sqlite` beside the executable. A ledger at the previous per-user location is copied across on first run (the original file is left untouched as a backup); if the folder beside the executable cannot be written, the per-user location is used as a fallback.
- **Ledger schema versioning.** The ledger records a schema version (`PRAGMA user_version`, now 1) and migrates explicitly on open. A ledger written by a newer schema is rejected with an explanation instead of being reset.
- **Command-line maintenance commands.** `--help`, `--version`, `--clean`, and `--uninstall`, with `--dry-run`, `--silent`, and `--json` modifiers and documented exit codes (0 success, 1 declined/refused/failed, 2 bad usage). Cleanup refuses to run while another instance is open and removes only the data at the location the current build uses.
- **Single-instance protection.** A second launch steps aside with an "already running" message instead of two copies sharing one ledger.
- **Startup recovery.** Jobs a previous session left in the `running` state are marked `interrupted` on launch and called out in the Recovery ledger panel; their files can still be reviewed and rolled back conservatively.
- **Execution hardening.** A staging mismatch during apply is now reported as an error (with the job failed cleanly) instead of aborting the process mid-job; files are flushed to disk before a copy is committed; plan files are saved atomically (temporary file, flush, rename) so a crash cannot leave a truncated plan.
- **Static runtime and release profile.** Release builds link the Microsoft C runtime statically (no separate VC++ redistributable needed) and use the pinned release profile (size-optimized, full LTO, single codegen unit, symbols stripped).
- **Release pipeline.** Windows CI enforces the formatting, lint, and test gate on every push and pull request. Tagged releases produce `FileWeaver-v<version>-x64-Setup.exe` (per-user NSIS installer), `FileWeaver-v<version>-x64-Portable.zip`, and `SHA256SUMS.txt`.

## 0.1.0

Initial foundation build.

- Pure planner for rename jobs: folder scan, literal prefix / literal replacement / counter rules, explicit numbering order (Path, Name, Modified oldest-first, JPEG Exif capture date oldest-first) with a stable path tie-break, sequence-gap warnings, and Windows filename validation (invalid characters, reserved DOS device names, trailing spaces/dots, the 255 code-unit filename limit, a conservative path-length limit, duplicate targets, and existing targets).
- Staged executor: same-volume rename cycles are staged through unique temporary names; copy and move are supported in the core, with cross-volume moves done as copy → size check → commit → source removal.
- Local SQLite operation ledger (WAL mode) recording jobs and per-item outcomes, a Recovery ledger panel that reviews recorded jobs against the current disk state, and conservative rollback that never overwrites a changed file.
- A Risk Budget line with every preview and per-row rule-stage provenance for every proposed name.
- Reviewable Plan Handoff: save a reviewed plan as a portable `.fileweaver-plan.json`, reopen it later with per-row drift checks, and apply only the rows that are still valid.
- Accessible Slint desktop shell: keyboard-focusable standard controls, accessibility descriptions, and status text.
