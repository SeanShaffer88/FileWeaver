# Credits

FileWeaver is written and maintained by Sean Shaffer (see [MAINTAINERS.md](MAINTAINERS.md)) and stands on these open-source components, each under its own license — the authoritative, version-pinned list is `Cargo.lock` in the repository root:

- **Slint** — the desktop UI toolkit and compiler (<https://slint.dev>).
- **rusqlite** with the bundled **SQLite** — the embedded operation ledger.
- **serde / serde_json** — serialization for plans, jobs, and the ledger payloads.
- **regex** — the regular-expression rule in the core pipeline.
- **kamadak-exif** — JPEG Exif capture-date reading.
- **time** — timestamps for the ledger and plan files.
- **walkdir** — recursive folder scanning.
- **thiserror** — typed error definitions.
- **tempfile** — test fixtures only (development dependency).

Comparable products informed the design research (Advanced Renamer, Bulk Rename Utility, PowerRename, ReNamer); the source-backed comparison is in [fileweaver-market-and-architecture.md](fileweaver-market-and-architecture.md). No code from those products is used.
