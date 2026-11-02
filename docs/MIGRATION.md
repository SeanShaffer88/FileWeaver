# Migration

## 0.1.0 → 0.2.0: the operation ledger moved

0.1.0 kept the operation ledger in the per-user state location
(`%LOCALAPPDATA%\FileWeaver\operations.sqlite` on Windows). 0.2.0 keeps it
in a portable data folder beside the executable:
`data\operations.sqlite`.

What happens on the first 0.2.0 launch:

1. If `data\operations.sqlite` already exists, it is used as-is.
2. Otherwise, if the folder beside the executable can be created and written, a ledger found at the old location — including any `-wal`/`-shm` sidecar files — is **copied** to `data\operations.sqlite`. The original file is left untouched as a backup.
3. If the folder beside the executable cannot be written (for example a read-only install location), the old per-user location is used as a fallback and nothing is copied.

Nothing is deleted during migration, and rollback history carries over with the copied ledger.

One consequence to know about: `--clean` removes only the data at the location the current build uses. If the migration backup still exists at the old location, it is not removed — and it will be copied back on the next launch. Delete that file manually to erase FileWeaver's data completely.

## Ledger schema versioning

The ledger records its schema version in SQLite's `PRAGMA user_version`:

- **Version 1** (0.2.0): the 0.1.0 tables, now version-stamped. Ledgers written by 0.1.0 report version 0 and are migrated in place (a stamp only; no data changes) the first time a 0.2.0+ build opens them.
- A ledger whose version is **newer** than the running build understands is rejected with an explanation. The file is never silently reset — rollback history is user data.

Every future schema bump must ship an explicit migration step and a migration fixture test, per the project's engineering contract.

## Plan files

`.fileweaver-plan.json` files are unchanged: format version 1 opens in both 0.1.0 and 0.2.0.
