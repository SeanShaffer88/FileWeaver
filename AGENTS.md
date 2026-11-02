# FileWeaver — Project Rules (condensed from Core Engineering Contract v2.6)

## Project Manifest
- tier: 3 (GUI app)
- tier rationale: Slint/femtovg multi-panel studio window (preview, rules, recovery ledger), layout beyond stock controls
- targets: windows-x64
- toolchain: Rust 2024, Slint 1.18, crt-static
- renderer: Slint femtovg (native engine direct renderer; static runtime linkage)
- release assets: FileWeaver-v{VERSION}-x64-Setup.exe + FileWeaver-v{VERSION}-x64-Portable.zip + SHA256SUMS.txt (no MSI — SEAN's amendment to the matrix)
- tier overrides: idle RAM cap 40 MB (t3 rule); LICENSE kept under that exact name for GitHub license detection — a documented deviation from the standard docs set's LICENSE.md
- dropped: Sections 4 and 5; Tier 1a/1b/2 release rows; non-Windows CI steps
- condensed from: master v2.6

## §0 Core (applies in full)
- Target OS baseline: Windows 10 (build 19041) minimum through Windows 11.
- Rust + `windows-sys`/raw FFI preferred; CRT linked statically (`crt-static`).
- Release profile: `opt-level = "s"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`, `strip = true`.
- `unwrap()`/`expect()` banned in hot paths and untrusted-input code; allowed only in `main`/startup and tests, each with an inline justification comment. All other errors: `match`/`?` with context.
- Every Win32 handle wrapped in a deterministic RAII type.
- **Zero cloud / zero telemetry:** fully functional air-gapped; zero telemetry, license-verification, or tracking endpoints, hardcoded.
- Single-instance mutex: `Global\FileWeaver-{9E1E458F-915B-4210-854B-9FE1AEF5E8AB}`.

## §1 Operational modes (summary)
- Confirm tier first; load only the t3 rule set before any analysis.
- Below 95% certainty on requirements/constraints for complex or irreversible work: stop and ask targeted questions first.
- Audit/review work uses the contract's standard modes (general review, bug hunt, security audit, pre-release audit); never claim a check passed without running it.

## §2 Workflow & release rules (t3 rows)
- Release shape: `FileWeaver-v{VERSION}-x64-Portable.zip` (mandatory) + `FileWeaver-v{VERSION}-x64-Setup.exe` + `SHA256SUMS.txt`. No MSI, ever (SEAN's amendment). x64 only.
- Releases (tag + GitHub Release) require explicit human confirmation; branches/PRs/commits do not.
- CI runs deterministic gates (fmt check, clippy `-D warnings`, full test suite) before any release; artifact names must match the matrix exactly; checksums generated in one pass.
- No external runtime package managers (no winget); static CRT so no VC++ redistributable is required.
- Config files carry a root `schema_version` (SQLite ledger: `PRAGMA user_version`); loaders migrate upward explicitly and never silently reset; every schema bump ships a migration fixture exercised in CI.
- UI: dark, compact, information-dense studio layout; mockup/wireframe confirmation before new top-level windows or layout changes above a single dialog/menu.
- Viewport: no window-level scrollbars at standard viewports; scrolling only inside virtualized data containers; persist geometry/splitters/filters and validate against `EnumDisplayMonitors` on launch.
- Tooltips mandatory for icon-only buttons, non-standard custom controls, status indicators, and column headers.
- Delivery: complete files for new files; surgical diffs for edits; concise status bullets.

## §3 The 30 characteristics (as they apply to t3)
1. Native code, static CRT, zero interpreter overhead.
2. Idle CPU ~0%; idle working set <40 MB (private working set, ≥60 s after startup, fresh Win10 19041 VM; no `EmptyWorkingSet` gaming). No busy-wait or sub-second polling; event primitives for hot paths; low-cadence work via waitable timers.
3. UI thread latency <16 ms; initial window render <50 ms; no splash screens or blocking startup.
4. Background work on a bounded pool/channel; worker threads at below-normal priority.
5. Bounded, memory-capped LRU caches; drain buffers under memory-pressure notifications.
6. Raw Win32/NT APIs over heavy wrappers.
7. (t1b tray integration — inert here.)
8. Portable builds write state only to the adjacent `data/` directory; zero writes to `%APPDATA%`/`%LOCALAPPDATA%`/`%PROGRAMDATA%`/`C:\Windows\Temp`.
9. Untrusted operations sandboxed in constrained child processes where applicable.
10. Engine decoupled from UI: headless CLI flags (`--headless`, `--json`, `--silent`, `--dry-run`); with `--json`, clean JSON on stdout, logs on stderr.
11. Per-Monitor DPI Awareness V2 manifest + Windows 10/11 compatibility GUIDs; follow system dark-mode changes without restart.
12. GUID-prefixed named mutex for single-instance; never hijack global keys or shared resources.
13. Zero handle/resource leaks; RAII everywhere; flat memory on long runs.
14. Disk mutations atomic: temp file → `FlushFileBuffers` → `ReplaceFileW`. DPAPI only for fixed-machine secrets; portable secrets need a user passphrase with authenticated derivation.
15. WAL/checkpoint recovery: interrupted work replays or resets safely on next launch.
16. Structured local logs with timestamps and configurable verbosity; never sent over the network.
17. Validate/sanitize every external buffer, CLI argument, registry entry, and config value before parsing.
18. Errors handled at worker boundaries; human-readable UI notices, never panic dialogs.
19. `--uninstall`/`--clean` purges every file, key, and registration the program created.
20. Temporary files scrubbed promptly (`FILE_FLAG_DELETE_ON_CLOSE` where suitable).
21. Disk writes only to the target path or local `data/`.
22. 100% offline; identical behaviour on air-gapped systems.
23. Settings in human-readable TOML/JSON; never bound to machine SID or hardware fingerprints.
24. Every background feature, auto-run entry, and hook has a visible settings toggle.
25. Cancellation (`CTRL_C_EVENT`, cancel buttons, `WM_CLOSE`) aborts within 50 ms, releasing locks and workers.
26. `--verbose`/`--debug`/`--trace` flags; every internal parameter inspectable and overridable.
27. Layout resizes without clipping across aspect ratios, ultra-wide, and high-refresh panels.
28. Common workflows simple; advanced flags/logs behind clear views; virtualized list controls for large data.
29. Real-time status readouts (bytes, items, MB/s) during intensive work; never hide stalls behind indeterminate spinners.
30. No unprompted toasts, promotional links, browser-default changes, or unapproved startup entries; offline help via Help view or `--help`.
