# NTFSearch

NTFSearch is a standalone, portable Windows file and folder finder built with `eframe`, with a runtime-selectable renderer path.

It indexes files and folders, supports live NTFS/USN updates when available, and includes a keyboard-first search panel with GPU and soft-renderer modes.

Read more: [NTFSearch on Substack](https://warped3.substack.com/p/ntfseach?utm_source=publication-search).

## Download and run

Download `rustsearch.exe` or the Windows x64 ZIP from the [latest release](https://github.com/IvRogoz/FunFixForWindowsSearch/releases/latest). Extract the ZIP, if used, into a writable folder and run `rustsearch.exe`. No installation is required. Keep the executable, its settings and `snapshots/` directory together when moving the app.

See the [changelog](CHANGELOG.md) for fixes and release validation.

## Run (dev)

```bash
cargo run
```

Renderer mode (optional):

- Default: GPU-native egui renderer
- Legacy soft ratatui renderer: run `cargo run -- --renderer=soft` or `rustsearch.exe --renderer=soft`

## Build (release)

```bash
cargo build --release
```

The release executable is `target/release/rustsearch.exe`. Copy it into a folder to use the app portably.

The app stores `scope.txt`, `quick-help-dismissed.txt`, the `snapshots/` directory, and (when enabled) `rustsearch-debug.log` beside `rustsearch.exe`. Keep these files and the executable together when moving the app. Run `rustsearch.exe --debug` to enable debug logging. No system environment variables are needed. Scope snapshots are tied to their resolved locations and replaced atomically. The live journal cursor stays in memory; older checkpoint and per-volume snapshot files are no longer used.

## Controls

- Backtick: show/hide panel (global hotkey)
- Type to search
- Arrow Up/Down, Page Up/Down, Home/End to navigate
- Enter open selected file or folder
- Alt+Enter reveal selected file or folder in Explorer
- Esc hide panel
- Slash commands: `/entire`, `/all`, `/x:`, `/up`, `/track`, `/latest`, `/last`, `/reindex`, `/rows`, `/testProgress`, `/about`, `/exit`
- Renderer commands: `/gpu` (GPU-native UI) and `/soft` (legacy soft-raster ratatui UI)

## Search syntax

- Plain text searches match file or folder names and full paths.
- Wildcards are supported with `*` and `?`, for example `*.rs` or `notes?.txt`.
- Boolean search supports standalone `AND` and `OR` operators:
  - `invoice AND pdf`
  - `invoice OR receipt`
  - `draft AND notes OR invoice AND pdf`
- `AND` binds within each `OR` group, so `a AND b OR c AND d` is evaluated as `(a AND b) OR (c AND d)`.
- Incomplete boolean expressions such as `AND`, `OR`, `name AND`, and `name OR` pause search until another term is entered.

## Slash commands

- `/entire`: search the entire current drive
- `/all`: search all local drives
- `/x:`: search a specific drive, for example `/d:`
- `/up`: relaunch elevated while preserving the current scope
- `/track`: toggle live event tracking
- `/latest [window]`: show recent changes, for example `/latest 30sec`
- `/last [window]`: alias for `/latest`
- `/reindex`: reindex the current scope
- `/rows N` or `/rows:N`: resize the panel for `N` result rows, clamped to 8-80 and the available work area; the setting survives fullscreen and full-height toggles
- `/fullscreen`: toggle fullscreen
- `/fullheight`: toggle full-height mode
- `/gpu`: switch to GPU renderer
- `/soft`: switch to soft renderer
- `/about`: show app information
- `/testProgress`: run the progress UI test
- `/exit`: quit the app

## Notes

- Folder results are marked with `[D]`.
- Live NTFS indexing is used when available; otherwise the app falls back to directory walking.
- Current-folder live indexing opens the containing drive and filters results back to the selected folder.
- A disconnected indexed drive keeps its cached results while the app retries; other available drives continue updating. Reconnection resumes the saved journal position when it is still valid, otherwise the affected volume is rebuilt. A failed or cancelled rebuild preserves the previous complete index.
- File-open and reveal failures appear in the status message. Results still being refreshed cannot be opened or revealed.

## Live USN regression test

Run `scripts/test-live-usn.ps1` from PowerShell to compile and run the explicitly ignored live integration test using the existing Rust 1.95.0 MSVC compiler and built dependencies. The default compiler path is under the current user's `.rustup` directory; use `-Compiler` to provide another already-installed compatible `rustc.exe`. The runner expects the existing libraries in `target/release/deps` and native Windows libraries in `target/validation-cargo-home`; it fails if they are missing and does not restore them. It requires permission to enumerate and read the workspace volume's existing NTFS journal; a restricted sandbox may deny that access. The script does not install packages, request UAC elevation, launch the UI, or access the desktop or screen.

The test creates its own files under `target/live-usn`, verifies file/folder changes, scope-boundary moves, search updates, 128-file bursts, idle behavior, snapshot replacement, and cancellation. Its executable, snapshots, and result log remain there for inspection; failed runs also retain their fixtures. Default unit-test runs skip this live test.

The regular suite also includes `indexing_ntfs::imp::recovery_simulations`. These in-memory tests run the production recovery loop with mocked device responses and snapshot writes. They cover journal resets, lost history, disconnection/reconnection, failed rebuilds, cancellation, handle cleanup, and continued updates from other drives, without accessing real drives or journals.

For v0.1.12, all 51 regular tests and the separate live-USN test passed. Actual journal reset and physical drive disconnection were not performed; those recovery paths were tested only through simulations.
