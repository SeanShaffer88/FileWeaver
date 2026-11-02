# Troubleshooting

| Symptom | Likely cause / recovery |
|---|---|
| **Apply is disabled** | The preview has blocking issues, no changes, or no folder was scanned. Read each row's status; the initial UI needs better issue filtering. |
| **A path is not scanned** | Check spelling, access permissions, and whether it is a symbolic link. The build deliberately skips symlinks/reparse-point-like links. |
| **"Target appeared after preview"** | Another program changed the folder after planning. Refresh the preview; the executor refuses to overwrite it. |
| **Recovery skips a file** | The destination's size/modification fingerprint differs from the recorded post-operation result, or its original location is occupied. The Recovery ledger's review shows which files are unchanged, changed, or missing before you roll back; preserve both files and resolve skipped ones manually. |
| **A job shows as "interrupted"** | A previous session ended (crash, kill, power loss) while that job was running. Its recorded items were not modified by the interruption marking; select the job in the Recovery ledger to review how its files look now before rolling anything back. |
| **"FileWeaver is already running"** | A second launch (or a `--clean`/`--uninstall` run) found the single-instance mutex held. Close the open FileWeaver window first. If no window is visible, a previous process may still be shutting down — wait a moment and retry. |
| **`--clean` says another instance is running** | Same cause: data deletion is refused while the app is open, because the running instance holds the ledger. Close FileWeaver and run the command again. |
| **"The operation ledger uses schema version N, which this build does not understand"** | The ledger was written by a newer FileWeaver build. Run the newer build, or move the ledger file aside to start with an empty history (rollback history in the moved file is preserved but unused). |
| **Settings/ledger not beside the executable** | The folder next to the executable could not be written, so the ledger fell back to the per-user location (`%LOCALAPPDATA%\FileWeaver\operations.sqlite`). Check the install folder's permissions, or use the portable zip from a writable folder. |
| **Behaviour on Linux/macOS** | Not a supported platform. The core and shell compile there and the core tests pass, which is useful during development, but the Windows filename rules and the Windows UX are neither meaningful nor validated off Windows. |
