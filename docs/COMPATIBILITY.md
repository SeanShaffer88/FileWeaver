# Compatibility

## Supported target

- **Windows 11 x64** is the product target.
- **Windows 10 x64 (build 19041 / 20H1 or later)** is an intended test target and the engineering baseline. It is not yet a verified support claim: no clean-machine Windows run has been recorded for the current build, so treat Windows 10 support as *Unverified* until testing is complete.
- The release executable is a single self-contained x64 binary: the Microsoft C runtime is linked statically, and SQLite is bundled. No separate runtime, database server, or browser engine is required.

## Not supported

- Windows 7 / 8.1, Windows Server, and ARM64 builds.
- Unusual network-filesystem semantics (the executor's staging and rename behaviour is designed and reasoned about for local NTFS volumes).
- Linux and macOS as end-user platforms. The core and shell compile there and the core tests pass, which is useful during development, but the Windows filename rules and the Windows user experience are neither meaningful nor validated off Windows.

## Data compatibility

- Ledgers written by 0.1.0 open in 0.2.0 and later: the file is copied to the portable `data\` location on first run and stamped with schema version 1. See [MIGRATION.md](MIGRATION.md).
- `.fileweaver-plan.json` files use plan format version 1; a plan addresses the same folder path it was saved from.
