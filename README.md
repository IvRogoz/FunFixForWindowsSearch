# NTFSearch

NTFSearch is the GPU-first rebuild of `wizmini-native` using `eframe` with a runtime-selectable renderer path.

It indexes files and folders, supports live NTFS/USN updates when available, and includes a keyboard-first search panel with GPU and soft-renderer modes.

## Run (dev)

```bash
cargo run --manifest-path RustSearch/Cargo.toml
```

Renderer mode (optional):

- Default: GPU-native egui renderer
- Legacy soft ratatui renderer: set `RUSTSEARCH_RENDERER=soft`

## Build (release)

```bash
cargo build --release --manifest-path RustSearch/Cargo.toml
```

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
- `/rows N` or `/rows:N`: resize the panel to show `N` result rows, clamped to 8-80
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
