# Conformance Audit — Core Engineering Contract v2.6 (tier t3)

Summary of the audit performed for the 0.2.0 conformance pass, and the
status of each finding. Evidence was the 0.1.0 source tree and a locally
run gate (formatting, clippy with warnings denied, full test suite — all
passing before and after the fixes).

| # | Finding (0.1.0 state) | Status in 0.2.0 |
|---|---|---|
| 1 | Tier undeclared: no `.project-tier`, no project `AGENTS.md` | **Fixed** — classified `t3`; `.project-tier` and a condensed `AGENTS.md` with the Project Manifest added; CI lints for the declaration |
| 2 | No static CRT configuration; README warned the VC++ runtime dependency was unverified | **Fixed** — `.cargo/config.toml` sets `+crt-static` for the MSVC target |
| 3 | Release profile below the contract's pinned profile (thin LTO, no opt-level, no panic setting) | **Fixed** — `opt-level = "s"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`, `strip = true` |
| 4 | Ledger stored under `%LOCALAPPDATA%`; no portable data mode | **Fixed** — ledger lives in `data\` beside the executable with one-way copy migration and a writability fallback; see [MIGRATION.md](MIGRATION.md) |
| 5 | No config/ledger schema version anywhere | **Fixed** — ledger carries `PRAGMA user_version` with an explicit migration function, a 0→1 fixture test, and rejection of newer schemas |
| 6 | No command-line surface at all (`main` never read argv) | **Fixed for maintenance scope** — `--help`/`--version`/`--clean`/`--uninstall` with `--dry-run`/`--silent`/`--json`, console attach, documented exit codes. Headless plan/apply remains future work (see [ROADMAP.md](ROADMAP.md)) |
| 7 | No single-instance protection; two copies could share one ledger | **Fixed** — GUID named mutex; second launch steps aside; maintenance commands refuse to run under a live instance |
| 8 | A panic (`expect`) sat in the executor's apply loop | **Fixed** — replaced with a typed `ExecuteError::StagingMismatch` and clean job failure |
| 9 | No crash reconciliation: jobs left `running` stayed `running` forever | **Fixed** — startup sweep marks them `interrupted`, surfaced in the Recovery ledger panel, with a regression test |
| 10 | No flush-to-disk on the copy-commit path; plan-file save was a plain non-atomic write | **Fixed** — `sync_all` before the commit rename; plan saves are temp-write → flush → rename |
| 11 | No CI, no release pipeline, no installer, no checksums | **Fixed** — Windows CI gate workflow; tag-driven release workflow producing Setup.exe + Portable.zip + SHA256SUMS.txt |
| 12 | No changelog; none of the standard project documents | **Fixed** — this docs set; BENCHMARK omitted (nothing is measured yet) |

## Still open (recorded, not hidden)

- Windows runtime behaviour, idle RAM against the tier's 40 MB cap, cold-start time, and DPI/manifest behaviour are **Unverified**: development and tests run off Windows.
- Window geometry is not persisted across launches (contract §2 viewport discipline).
- The preview list is not virtualized; very large folders may render slowly (contract §2/§3 #28).
- The shell ships a light theme; the contract §2 dark studio baseline is not yet met.
