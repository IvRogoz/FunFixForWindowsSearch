# Architecture (v1)

## Product shape

- Background tray app
- Global hotkey toggles top overlay search panel
- TUI-like keyboard-first interaction

## Runtime components

- `native-app` (Iced + Win32): process lifecycle, tray, hotkeys, overlay window
- `wizd`: orchestrates indexing and query pipeline
- `wizcore-index`: keeps in-memory file metadata index
- `wizcore-watch`: applies live file system deltas
- `wizcore-query`: parses search query and returns ranked result ids
- `wizcore-shell`: opens file/reveals path/copies path

## UI direction

- Native Rust UI using Iced
- TUI-like overlay styling (dense rows, keyboard-first)
- No webview/Web UI runtime

## Data flow

1. App starts in tray and begins index bootstrap.
2. User presses global hotkey.
3. Overlay slides from top and focuses input.
4. Iced app handles keyboard events (`Esc`, arrows, `Enter`) and query changes.
5. Query engine streams chunks for current request.
6. UI discards stale request chunks and renders latest request only.
7. Enter triggers `open_item`.

## Current native runtime behavior

- Borderless, always-on-top, skip-taskbar overlay window anchored at top-left.
- Global hotkey (default backtick) toggles panel visibility.
- Tray menu supports show/hide and quit.
- External events are polled at a low fixed interval; animation uses frame subscription only while moving.

## V1 constraints

- Local NTFS volumes first
- Filename/path search first
- Real-time deltas via USN when available; fallback polling otherwise

## Latency budgets

- Panel show/hide: 120-180 ms
- Warm query initial results: < 50 ms
- Tray startup: < 1.5 s
