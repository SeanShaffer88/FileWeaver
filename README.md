# FileWeaver (early implementation)

> A local-first Windows batch-renaming and file-organization studio designed to make every proposed file change visible, explainable, and recoverable.

**Status: version 0.1.0 — foundation / review build, not production-ready.** The core planner, safe rename executor, local SQLite operation ledger, and an accessible Slint desktop shell are implemented. The full professional workflow described in [`docs/fileweaver-market-and-architecture.md`](docs/fileweaver-market-and-architecture.md) is **not** complete yet.

## What this build does

- Scans a folder without changing anything. By default only files directly in the selected folder are included; with **Include subfolders** enabled, the scan covers the entire folder tree at any depth.
- Builds a preview for literal prefix, literal replacement, and counter rules. The preview is **not** live: it updates only when you run Scan or Refresh preview, and Apply always executes the most recent preview — refresh after changing any rule before applying. In the shell the counter is fixed (underscore separator, starting at 1, four digits); the core rule accepts other separators, starts, and widths. The desktop shell applies renames only — copy and move exist in the core API but are not exposed in the UI.
- Makes numbering order explicit (the **Order Observatory**): the shell offers Path, Name, Modified (oldest first), and Capture date (oldest first, from JPEG Exif) as the numbering/processing order, states the chosen order and the assigned counter range with every preview, and records the order in the plan and journal. Every order falls back to the stable path key on ties. The planner also warns about possible **sequence gaps** — holes in a numbered pattern (for example `IMG_0001, IMG_0002, IMG_0004`) in the proposed names; a gap is a warning, never a block, because it can be legitimate.
- Validates empty names, invalid Windows filename characters, reserved DOS device names, trailing spaces/dots, the 255 UTF-16 code-unit filename-component limit, a conservative legacy path-length limit (destination paths over 259 UTF-16 code units are blocked), duplicate targets, and existing unselected targets.
- Runs planning and filesystem work off the UI event loop.
- Applies same-volume rename cycles safely by staging source files using unique temporary names.
- Supports copy/move logic in the core API; cross-volume moves use copy-to-unique-temp, size verification, commit, then source deletion.
- Records applied items in a local SQLite operation ledger and supplies conservative rollback in the core API. Rollback rejects files whose recorded post-operation fingerprint has changed.
- Shows a **Risk Budget** line with every preview: how many files will change, are blocked, need review, or stay unchanged — and that the plan overwrites nothing.
- Provides a **Recovery ledger** panel in the shell: recent jobs can be selected to review how their files look now (unchanged, changed, or missing, using the same fingerprint check rollback performs), then rolled back behind a two-step confirmation. Rollback restores only unchanged files whose original location is still free; changed or missing files are never overwritten.
- Explains every proposed name: selecting a preview row shows its **rule-stage provenance** — the original name followed by the result of each rule in order (for example `photo.txt → Add prefix: 2026-photo.txt → Append counter: 2026-photo_0001.txt`) — using the stage results the rule engine records when the plan is built.
- Reads JPEG Exif capture timestamps (`.jpg`/`.jpeg`, `DateTimeOriginal` then `DateTimeDigitized`) while planning. The core date rule can use them; the first UI slice does not expose that rule yet.
- Exposes Slint accessibility metadata for the implemented controls, keyboard-focusable standard controls, and status text.

## Intentionally not yet exposed or complete

- Native file/folder picker and drag-and-drop intake (paste a path in the current shell).
- Per-row inclusion/exclusion, issue filters, sortable columns, and thumbnails.
- UI controls for the regex, case, and date/Exif rules and for copy/move destination templates. Those exist in the core API, but none should be represented as complete UI features.
- CSV/JSON mapping, saved presets, and report export. These have **no implementation yet**, in the core or the UI.
- Folder mutation, watched folders, Explorer context menu, portable-data mode, CLI automation, hash/duplicate analysis, metadata writing, document-content classification, integrations, and networking.
- Stable Windows file IDs for rollback. This build uses a conservative size + modification-time fingerprint, so it may reject safe recovery and cannot prove identity against an adversarial or coincidental replacement.
- Windows VM testing, real Windows screen-reader testing, high-contrast testing, installer creation, code signing, or clean-machine runtime verification.

## Requirements

### End users (planned release)

The selected Slint architecture does not require a browser engine, Java, Python, a database server, an account, network access, or administrator service for normal use. SQLite will be bundled in the application through `rusqlite`'s bundled SQLite feature.

The final signed per-user installer will be responsible for including any Microsoft Visual C++ runtime dependency if the release build needs it. This has **not** been verified yet, so do not treat this README as an installer promise.

### Developers

- Rust **1.92+** (the project currently builds with Rust 1.99.1).
- A C/C++ build toolchain suitable for the selected Rust target; bundled SQLite is compiled as part of the dependency build.
- For Windows x64 releases: the MSVC Rust target, Visual Studio Build Tools / Windows SDK, and a Windows 10/11 x64 test environment.

The product target is **Windows 11 x64**. Windows 10 x64 is an intended test target, not a support claim until testing is complete. Windows 7/8.1, Windows Server, ARM64, and unusual network filesystem semantics are not supported claims.

## Build and verify

```powershell
# Windows PowerShell, after installing Rust/MSVC prerequisites
rustup target add x86_64-pc-windows-msvc
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release --target x86_64-pc-windows-msvc
```

There is currently no CI enforcing these commands — they are the contributor's local gate. The automated test suite is small (7 tests), and several validation paths are currently covered by code inspection only.

During development on a supported desktop platform:

```powershell
cargo run
```

The current shell accepts a manually pasted folder path, optional subfolder scanning, and simple rules. Use a disposable copy of files only: this is an early implementation.

## Safety model

1. **Plan first.** `plan()` is pure: it scans metadata and proposes actions but never modifies files.
2. **Block by default.** Existing targets and within-batch duplicates block application. There is no overwrite policy.
3. **Stage rename/move sources.** Every rename and move is first staged through a unique temporary name in the source folder before its destination is materialized, which is also what makes file swaps and case-only rename sequences safe.
4. **Journal durable state.** A job row is written before filesystem changes, and item outcomes are committed locally as changes complete.
5. **Undo conservatively.** Rollback compares the destination’s recorded post-operation fingerprint before touching it. If the file appears to have changed, it is skipped with an explanation.
6. **Never hide partial work.** Cancellation occurs between items; completed items remain in the ledger and unmaterialized staging files are restored when possible.

Cross-volume moves cannot be atomic. They are implemented as copy → byte-count check → non-overwriting commit → source removal, and should be prominently labeled in the future UI.

## Local data and privacy

- **Windows location:** `%LOCALAPPDATA%\FileWeaver\operations.sqlite`
- **Other development platforms:** `$XDG_STATE_HOME/FileWeaver/operations.sqlite`, then `~/.local/state/FileWeaver/operations.sqlite`
- The path is resolved from the first of those variables that is set (on Windows, if `LOCALAPPDATA` is unset the same fallback chain applies). If none is set, the ledger is created relative to the process working directory as `./FileWeaver/operations.sqlite`.
- The current build has no networking, telemetry, update checking, account, synchronization, upload, or online metadata lookup.
- The operation ledger contains file paths and outcome metadata. Treat it as sensitive. The ledger uses SQLite's WAL mode, so `operations.sqlite-wal` and `operations.sqlite-shm` may sit alongside it while the app runs; close the app before copying it as a backup so all state is folded back into the main file. Deleting it removes rollback history.

## Project layout

```text
src/lib.rs        Core crate root (fileweaver_core) and its public API re-exports
src/domain.rs     Data model, issue taxonomy, plan/request records
src/rules.rs      Deterministic pure naming-rule pipeline
src/metadata.rs   Narrow JPEG/Exif reader that tolerates malformed files
src/planner.rs    Non-destructive scan, stable order, validation, collision detection
src/journal.rs    SQLite operation ledger
src/executor.rs   Staged execution, cancellation, conservative rollback
src/main.rs       Slint desktop integration and background work scheduling
build.rs          Compiles ui/main.slint into the binary via slint-build
ui/main.slint     Accessible desktop shell
```

## Troubleshooting

| Symptom | Likely cause / recovery |
|---|---|
| **Apply is disabled** | The preview has blocking issues, no changes, or no folder was scanned. Read each row’s status; the initial UI needs better issue filtering. |
| **A path is not scanned** | Check spelling, access permissions, and whether it is a symbolic link. The current build deliberately skips symlinks/reparse-point-like links. |
| **“Target appeared after preview”** | Another program changed the folder after planning. Refresh the preview; the executor refuses to overwrite it. |
| **Recovery skips a file** | The destination’s size/modification fingerprint differs from the recorded post-operation result, or its original location is occupied. The Recovery ledger’s review shows which files are unchanged, changed, or missing before you roll back; preserve both files and resolve skipped ones manually. |
| **Behaviour on Linux/macOS** | Not a supported platform. The core and shell compile there and the core tests pass, which is useful during development, but the Windows filename rules and the Windows UX are neither meaningful nor validated off Windows. |

## License

This repository is licensed under the **MIT License**. See [`LICENSE`](LICENSE).

## Research and design record

See [`docs/fileweaver-market-and-architecture.md`](docs/fileweaver-market-and-architecture.md) for source-backed competitor research, a separate feature inventory, feature-by-feature decisions, original feature hypotheses, framework comparison, privacy/installer plan, and verification plan.
