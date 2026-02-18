# WizMini (working name)

Tray-resident, keyboard-first Windows file finder inspired by WizFile, optimized for minimal UI and fast response.

## Current status

Initial monorepo scaffold with:

- Native Rust desktop shell using Iced (no web stack)
- Tray icon + global hotkey (default backtick) wiring
- TUI-like panel skeleton with keyboard navigation
- Rust core crates split by responsibility
- IPC contract and v1 architecture docs

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
- Search box filters indexed files from the active scope.
- Keyboard controls: `ArrowUp/ArrowDown` select, `Enter` select/open, `Alt+Enter` reveal, `Esc` hide.

## Slash commands

- `/entire` - set scope to the entire current drive (persistent until changed)
- `/all` - set scope to all local drives (persistent until changed)
- `/x:` - set scope to a specific drive (example: `/d:`), persistent until changed
- `/testProgress` - visual progress bar test only (no indexing)

Behavior notes:

- Type `/` to open command suggestions.
- Use `ArrowUp/ArrowDown` to select a command and `Enter` to apply it.
- If `/` is removed, arrow keys return to normal file-result navigation.

## Notes

- This scaffold focuses on architecture and interfaces first.
- NTFS MFT/USN integrations and real file open/reveal actions are next.
