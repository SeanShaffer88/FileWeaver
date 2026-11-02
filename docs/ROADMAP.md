# Roadmap

Direction only — nothing here is a release commitment. The researched
backlog behind this list is [innovation-proposals.md](innovation-proposals.md);
the follow-on concepts ranked there (collection sequence memory, Rehearsal
Mode, Collection Lint) remain candidates.

## Shell completeness (core capability the UI does not expose yet)

- UI controls for the regex, case, and date/Exif rules that already exist in the core API.
- Copy/move destination controls (the core executes them; the shell applies renames only).
- Native file/folder picker and drag-and-drop intake (the shell currently takes a pasted path).
- Per-row inclusion/exclusion, issue filters, sortable columns, and thumbnails.
- Full counter controls in the shell (separator, start, width).

## New capability with no implementation yet

- CSV/JSON mapping, saved presets, and report export.
- Folder mutation, watched folders, and an Explorer context menu.
- Hash/duplicate analysis and metadata writing.
- Headless plan/apply automation (the 0.2.0 command line covers maintenance only: help, version, cleanup).
- Document-content classification.

## Hardening and verification

- Stable Windows file IDs for rollback identity, replacing the size + modification-time fingerprint.
- Window-geometry persistence across launches.
- Virtualized preview list for very large folders.
- Windows VM testing, real Windows screen-reader testing, high-contrast testing, and clean-machine verification of the installer and runtime.
- Code signing for release builds.
