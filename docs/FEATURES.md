# Features

What the current build does. Anything not listed here is not in the build;
planned work is listed in [ROADMAP.md](ROADMAP.md).

## Planning (never changes files)

- Folder scan, optionally including subfolders at any depth. Symbolic links are deliberately skipped.
- Rules: literal prefix, literal replacement, and a counter (separator, start, and width are configurable in the core; the shell fixes underscore / start 1 / width 4).
- Numbering order is explicit — Path, Name, Modified (oldest first), or JPEG Exif capture date (oldest first, `DateTimeOriginal` then `DateTimeDigitized`) — with a stable path tie-break, stated with every preview and recorded in the plan and the ledger.
- Sequence-gap warnings over the proposed names (a warning, never a block).
- Validation: empty names, invalid Windows filename characters, reserved DOS device names, trailing spaces or dots, the 255 UTF-16 code-unit filename-component limit, a conservative legacy path-length limit (destinations over 259 code units are blocked), duplicate targets, and targets that already exist outside the selection.
- Risk Budget line with every preview: how many files change, are blocked, need review, or stay unchanged — and that the plan overwrites nothing.
- Rule-stage provenance per row: the original name followed by the result of each rule in order.

## Applying

- Same-volume renames and moves are staged through unique temporary names, so swaps and case-only rename sequences are safe.
- Copy and move exist in the core API (the shell currently applies renames only). Cross-volume moves are copy → byte-count check → flush → non-overwriting commit → source removal.
- No overwrite policy, ever: an existing target blocks the item.
- Planning and filesystem work run off the UI event loop; a job can be stopped between items.

## Recovery

- Every applied job is recorded in a local SQLite operation ledger (WAL mode) with per-item outcomes and post-operation fingerprints (size + modification time).
- Recovery ledger panel: review any recorded job against the current disk state (unchanged, changed, or missing), then roll back behind a two-step confirmation. Rollback restores only unchanged files whose original location is still free.
- Jobs left `running` by a crashed session are marked `interrupted` at the next launch and called out in the panel.
- Reviewable Plan Handoff: save a reviewed, applicable preview as a `.fileweaver-plan.json` (names and paths only — not a backup), reopen it later with per-row drift labels (unchanged, changed, missing, result-differs, target-now-exists), and apply only the still-valid rows.

## Data and maintenance

- The ledger lives in `data\operations.sqlite` beside the executable, with a recorded schema version and explicit migrations; see [MIGRATION.md](MIGRATION.md).
- Command-line maintenance: `--help`, `--version`, `--clean`, `--uninstall` (with `--dry-run`, `--silent`, `--json`). See the README's local-data section and `fileweaver --help`.
- Single-instance protection: a second launch steps aside instead of sharing the ledger.
- No networking, telemetry, accounts, update checks, or online lookups of any kind.
