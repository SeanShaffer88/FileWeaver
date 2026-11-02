# Architecture

FileWeaver is a single Rust crate with a strict split between a pure core
library and the desktop shell. The design records behind these decisions
are [fileweaver-market-and-architecture.md](fileweaver-market-and-architecture.md)
and [innovation-proposals.md](innovation-proposals.md).

## Layout

```text
src/lib.rs        fileweaver_core: public API re-exports
src/domain.rs     Data model, issue taxonomy, plan/request records
src/rules.rs      Deterministic pure naming-rule pipeline (records per-stage results)
src/metadata.rs   Narrow JPEG/Exif reader that tolerates malformed files
src/planner.rs    Non-destructive scan, stable ordering, validation, collision detection
src/journal.rs    SQLite operation ledger (schema-versioned, WAL)
src/executor.rs   Staged execution, cancellation, conservative rollback
src/handoff.rs    .fileweaver-plan.json format and drift evaluation
src/main.rs       Slint desktop shell and background work scheduling
src/cli.rs        Command-line maintenance commands (--help/--version/--clean/--uninstall)
src/win32.rs      Minimal Windows FFI: parent-console attach, single-instance mutex
build.rs          Compiles ui/main.slint into the binary via slint-build
ui/main.slint     Accessible desktop shell definition
```

## Rules the architecture enforces

1. **Planning is pure.** `plan()` reads metadata and returns a `PlannedJob`; it never modifies a file. Every safety check (collisions, reserved names, length limits) lives here, so the executor can trust a validated plan.
2. **The journal is written before the files.** A job row and its planned items are committed to the ledger before the executor touches the first source file, and item states are updated as work completes. A crash therefore always leaves an explainable record.
3. **Staging makes renames safe.** Rename/move sources are first moved to unique temporary names in the same folder; destinations are then materialized without overwriting anything. Cross-volume moves fall back to copy → size check → flush → commit → source removal.
4. **Rollback is conservative.** It compares each destination's current size + modification-time fingerprint with the recorded post-operation fingerprint and skips — with an explanation — anything that changed or whose original location is occupied.
5. **The shell is a client of the core.** All scanning, applying, reviewing, and rollback run through `fileweaver_core` on background threads; the Slint UI only renders state and forwards intent. The CLI maintenance commands use the same ledger and path resolution as the GUI.
6. **One instance, one ledger.** A named mutex (`Global\FileWeaver-{9E1E458F-915B-4210-854B-9FE1AEF5E8AB}`) ensures a second launch steps aside, and maintenance commands refuse to delete data while an instance holds it.

## Data

- Ledger: `data\operations.sqlite` beside the executable (fallback: the per-user state location when that folder is not writable). SQLite in WAL mode; schema version in `PRAGMA user_version` (currently 1), migrated explicitly in `Journal::open`.
- Plan files: user-chosen paths, written atomically (temporary sibling, flush, rename). Format version 1, JSON.
- No other persistent state: no registry settings, no configuration files, no caches.
