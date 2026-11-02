# Install

## From a release

Each release provides three files (see `SHA256SUMS.txt` in the release for checksums):

- **`FileWeaver-v<version>-x64-Setup.exe`** — per-user installer (NSIS). It installs to `%LOCALAPPDATA%\Programs\FileWeaver`, adds a Start menu shortcut and an Add/Remove Programs entry, and needs no administrator rights. It registers no services, shell extensions, or autostart entries.
- **`FileWeaver-v<version>-x64-Portable.zip`** — unzip anywhere and run `fileweaver.exe`. Nothing is installed.

The installer is not code-signed, so Windows SmartScreen may warn on first run.

FileWeaver keeps its operation ledger in a `data` folder next to the executable (see [MIGRATION.md](MIGRATION.md) and PORTABLE.txt in the portable zip). Neither the installer nor the uninstaller ever deletes that folder.

## Uninstall

Remove the program from Windows Settings → Apps, or run `Uninstall.exe` in the install folder. Your `data` folder (the operation ledger and rollback history) is left in place on purpose. To erase it as well, run `fileweaver --clean` before uninstalling, or delete the `data` folder manually afterwards.

## From source

Prerequisites: Rust **1.92+** and a C/C++ build toolchain (bundled SQLite compiles as part of the dependency build). For the Windows x64 release build: the MSVC Rust target and Visual Studio Build Tools / Windows SDK.

```powershell
rustup target add x86_64-pc-windows-msvc
cargo build --release --target x86_64-pc-windows-msvc
```

The contributor gate (the same commands CI runs) is:

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```
