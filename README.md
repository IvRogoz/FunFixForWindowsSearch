# WizMini (working name)

Tray-resident, keyboard-first Windows file finder inspired by WizFile, optimized for minimal UI and fast response.

## Current status

Working native prototype with:

- Native Rust desktop app using Iced (no web stack)
- Tray icon + global hotkey (default backtick) show/hide flow
- Scope-aware indexing (NTFS live path on Windows, dirwalk fallback when not elevated)
- Debounced, wildcard-capable search UI with keyboard-first navigation
- Slash-command workflow for scope, elevation, tracking toggle, recent-changes filtering, and reindexing

## Stack

- Rust (indexing/query/watch core)
- Iced 0.14 (native Rust GUI)
- `global-hotkey` + `tray-icon` for native desktop integration

## Repo layout

```
apps/
  native/            # Iced native desktop app shell
crates/
  wizcore-config/    # settings models + persistence contracts
  wizcore-index/     # initial index model + indexing contracts
  wizcore-query/     # query parser + scoring contracts
  wizcore-shell/     # open/reveal/copy shell actions contracts
  wizcore-watch/     # file system change watch contracts
  wizd/              # orchestration service contracts
docs/
  architecture.md
  ipc.md
```

## Prerequisites

Install these tools before running the app:

- Rust: `rustup`, `rustc`, `cargo`
- MSVC build tools

## Suggested next commands

After installing Rust tooling:

```bash
cargo check --workspace
cargo run -p wizmini-native
```

## What you can test now

- App runs as a native windowed process with tray icon.
- Global hotkey default is backtick (`) to show/hide panel.
- Tray menu supports `Show/Hide` and `Quit`.
- Search box filters indexed files from the active scope (NTFS volume indexing for drive scopes on Windows; directory walk fallback in non-elevated mode).
- Wildcards are supported in search: `*` (any sequence) and `?` (single character), e.g. `sraz*`.
- Search execution is debounced so typing stays responsive.
- While typing is active, in-flight searches are cancelled and no new search starts until input settles.
- Filename lookups use an in-memory accelerator (exact + short-prefix index) for faster filename queries.
- Search input is intentionally disabled while indexing progress is active.
- Keyboard controls: `ArrowUp/ArrowDown` select, `Enter` select/open, `Alt+Enter` reveal, `Esc` hide.
- Results list follows keyboard selection scrolling; file names are colorized by type, and long paths are middle-truncated with `...`.
- Panel width is dynamic (about half the screen width, clamped), and `/exit` is highlighted in bold red in command suggestions.
- Status line shows current scope, memory estimate for the in-memory index, and live delta counters (`+added ~updated -deleted`).

## Slash commands

- `/entire` - set scope to the entire current drive (persists across restarts)
- `/all` - set scope to all local drives (persists across restarts)
- `/x:` - set scope to a specific drive (example: `/d:`), persists across restarts
- `/up` - relaunch app elevated (Windows UAC prompt)
- `/track` - toggle live event tracking on/off
- `/latest [window]` - show files changed recently from USN timestamps (default `5m`; examples: `/latest 30sec`, `/latest 1m`, `/latest 3h`)
- `/last [window]` - alias of `/latest`
- `/reindex` - force reindex of current scope
- `/testProgress` - visual progress bar test only (no indexing)
- `/exit` - exit the app immediately

Behavior notes:

- Type `/` to open command suggestions.
- Use `ArrowUp/ArrowDown` to select a command and `Enter` to apply it.
- If `/` is removed, arrow keys return to normal file-result navigation.
- Unknown slash commands do not open files.
- `/latest` and `/last` are available only when tracking is enabled.

## Notes

- NTFS MFT-based volume enumeration is used for drive scopes on Windows.
- NTFS USN journal replay keeps drive-scope index data updated after initial load.
- USN checkpoints and debug logs are persisted under `%LOCALAPPDATA%\WizMini`.
- Scope index snapshots are persisted in binary format (`.bin`) under `%LOCALAPPDATA%\WizMini\snapshots`.
- The in-memory file list is optimized for memory pressure by storing path-first entries and deriving display filename from path.
- Command/query helpers are split into `apps/native/src/commands.rs` and `apps/native/src/search.rs`.
