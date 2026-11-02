# Contributing

## Prerequisites

- Rust **1.92+** (the project currently builds with Rust 1.99.1).
- A C/C++ build toolchain suitable for the Rust target; bundled SQLite compiles as part of the dependency build.
- For Windows release work: the `x86_64-pc-windows-msvc` target and Visual Studio Build Tools / Windows SDK.

## The gate

Every change must pass the same gate CI runs — locally, before pushing:

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

## Project rules worth knowing

- **Planning stays pure.** `plan()` must never modify a file; filesystem mutation belongs to the executor, behind a validated plan.
- **No overwrite path may be added.** Blocking is the only collision policy the executor implements; a design that needs overwriting needs a different conversation first.
- **The journal is written before the files**, and every ledger schema change bumps `PRAGMA user_version` with an explicit migration and a migration fixture test (see [MIGRATION.md](MIGRATION.md)).
- **No new dependencies for convenience.** The dependency set is deliberately small, offline, and web-free; see the justification standard in [../AGENTS.md](../AGENTS.md).
- Keep user-facing claims honest: if a behaviour is only covered by code inspection, docs say so rather than implying it was tested on Windows.
- Public documentation contains product facts only — no process commentary.

## Pull requests

Keep changes small and focused, describe the behaviour change and how it was verified (which gate commands ran, what remains unverified), and update the changelog's Unreleased section when users would notice the change.
