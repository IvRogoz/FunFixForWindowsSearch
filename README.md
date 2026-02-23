# WizMini

WizMini is a native, keyboard-first Windows file finder. It runs in the tray, opens with a global hotkey, and focuses on fast filename/path lookup with NTFS + USN live updates when available.

For the long-form architecture write-up, rationale, and deep technical explanation, see `post.md`.

## Requirements

- Rust toolchain (`rustup`, `cargo`, `rustc`)
- Windows MSVC build tools

## Run (dev)

From repo root:

```bash
cargo check --workspace
cargo run -p wizmini-native
```

From anywhere:

```bash
cargo run -p wizmini-native --manifest-path D:\Programing\WizMiini\FunFixForWindowsSearch\Cargo.toml
```

## Build (release)

```bash
cargo build --release -p wizmini-native
```

Output binary:

`target/release/wizmini-native.exe`

## Basic usage

- Press `` ` `` to show/hide panel
- Type to search
- `ArrowUp` / `ArrowDown` to move selection
- `PageUp` / `PageDown` / `Home` / `End` for fast navigation
- `Enter` open file
- `Alt+Enter` reveal in Explorer
- `Esc` hide panel
- Type `/` for command mode

### Overlay behavior

- When Quick Start is visible, `Enter` confirms the selected action (`Close` or `Don't show again`).
- Non-elevated warning overlay dismisses on any key press.

## Slash commands

- `/entire` scope to current drive
- `/all` scope to all local drives
- `/x:` scope to specific drive (example: `/d:`)
- `/up` relaunch elevated
- `/track` toggle live tracking
- `/latest [window]` recent changes filter
- `/last [window]` alias of `/latest`
- `/reindex` force reindex
- `/testProgress` progress UI test
- `/exit` quit app
