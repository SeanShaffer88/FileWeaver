# Security

## Security model

- **Offline by design.** FileWeaver has no networking, telemetry, update checking, account system, synchronization, upload, or online metadata lookup. It behaves identically on an air-gapped machine.
- **No overwrite, ever.** The planner blocks any action whose target exists; the executor stages renames through unique temporary names and commits without overwriting. Rollback compares a recorded size + modification-time fingerprint before touching a file and skips anything that changed.
- **Conservative fingerprint limitation.** Rollback identity uses size + modification time, not stable Windows file IDs. That can reject a safe recovery, and it cannot prove identity against a deliberate or coincidental replacement with matching size and timestamps. Treat rollback as a strong convenience, not a security boundary.
- **Local data sensitivity.** The operation ledger (`data\operations.sqlite`) contains the paths and outcomes of every applied job. Treat it as sensitive: it is not encrypted, and anyone who can read the folder can read your rename history. Deleting the ledger (or running `fileweaver --clean`) erases that history — and with it, the ability to roll jobs back.
- **Plan files.** `.fileweaver-plan.json` files contain file names and paths only, never file contents. Review them before sharing, as paths can themselves be sensitive.
- **Privileges.** FileWeaver runs with ordinary user rights and never asks for administrator elevation. The installer is per-user for the same reason. Builds are not code-signed.

## Reporting a vulnerability

Report security problems through the GitHub issue tracker (see [SUPPORT.md](SUPPORT.md)). For a problem that would put users' files at risk if discussed publicly, say so in a minimal issue first so details can be exchanged with care.
