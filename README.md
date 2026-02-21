# WizMini

WizMini is a keyboard-first Windows file finder designed to feel immediate. It lives in the tray, appears on a global hotkey, and treats search as an interaction loop that should keep up with human typing instead of making the user wait for every keystroke.

## What inspired this app

The project is inspired by tools such as WizFile and Everything, and by the old command-palette idea from editors where you type, narrow, and execute without context switches. The design goal is not to replicate Windows Explorer search, but to create a focused launcher-like flow for local files where opening and revealing items is the primary job.

This is also a response to a common Windows experience: search often feels acceptable in one moment and unexpectedly slow in the next. That inconsistency is what this app tries to remove.

## Why Windows search often feels slow

Windows Search is built to solve a broad problem. It combines content indexing, metadata extraction, policy boundaries, background scheduling, and integration with many system surfaces. That breadth is useful, but it also means the system can spend time on work that is not directly related to your immediate filename lookup.

When users perceive slowness, they are usually feeling one of three things. They are waiting on I/O because the query path touches disk or cold caches. They are seeing scheduler variability because indexing and query work compete with other system activity. Or they are paying feature overhead because the search stack tries to be universal rather than optimized for a narrow, fast local-file interaction.

WizMini chooses a narrower target. It focuses on local path and filename discovery, keeps a compact in-memory representation hot, and updates that representation incrementally. The tradeoff is intentional specialization in exchange for lower and more stable interactive latency.

## Why this app works in practice

The core idea is simple: do heavier work in the background, then keep foreground query work cheap. At startup, the app builds or loads an index for the active scope. During use, it answers queries from memory, not by walking the filesystem on each keystroke. As changes happen, the index is updated with deltas so it remains current.

This architecture gives responsive typing because query execution is mostly CPU and memory bound, not storage bound. It also gives predictable behavior under load because stale query chunks can be dropped and in-flight searches can be cancelled while the user continues typing.

On Windows drive scopes, NTFS metadata and USN-driven change tracking are used to keep state aligned with the filesystem. When elevated access is not available, the app falls back to directory walking so it remains functional, then upgrades behavior when privileges allow deeper system access.

## How the app behaves at runtime

WizMini runs as a native Rust desktop process using Iced. The tray icon controls visibility and lifecycle. The default global hotkey is backtick, which toggles the overlay panel. The overlay is borderless, always on top, and optimized for keyboard navigation.

Typing in the query field starts a debounced search pass. Wildcards are supported, with `*` matching any sequence and `?` matching a single character. While typing is active, active searches are cancelled and restarted only after input settles, which reduces UI hitching and keeps interaction smooth.

Result navigation stays keyboard-centric. Arrow keys move selection, Enter opens, Alt+Enter reveals in Explorer, and Escape hides the panel. Long paths are compacted in the middle, and the status line shows scope, memory estimate, and live delta counters so the user can see index health in real time.

Recent UX updates make keyboard flow easier to read and learn. Selection now uses a larger arrow marker, list navigation supports `PageUp`, `PageDown`, `Home`, and `End`, and progress copy clearly distinguishes full index builds from incremental refresh passes.

On first run, the app can show a Quick Start overlay that explains the essential controls, including backtick to show or hide, Enter and Alt+Enter actions, Escape behavior, and slash-command usage. Users can dismiss it once or choose a persistent `Don't show again` preference.

## Slash command language

Commands are entered directly in the search input by starting with `/`. When command mode is active, the app shows suggestions and routes Enter to command execution instead of file open. If the slash prefix is removed, input immediately returns to normal query mode.

`/entire` sets the scope to the entire current drive and persists that choice across restarts. `/all` switches to all local drives and persists. `/x:` selects a specific drive such as `/d:`. `/up` relaunches with elevation through UAC. `/track` toggles live change tracking. `/latest [window]` and `/last [window]` filter results to recently changed files, with a default window of five minutes and examples like `30sec`, `1m`, or `3h`. `/reindex` forces a full rebuild of the current scope. `/testProgress` runs a visual progress test without indexing work. `/exit` closes the app immediately.

Unknown slash commands are handled safely and do not accidentally open the currently selected file.

## Architecture in narrative form

The runtime is split into a native shell and a data pipeline. The native shell owns process lifecycle, tray integration, hotkeys, panel animation, command mode, and result rendering. The data pipeline owns indexing, delta application, query parsing, ranking, and shell actions for open or reveal.

Inside the repository, `apps/native` contains the Iced application. The core behavior is gradually organized into dedicated crates under `crates`, including configuration contracts, index contracts, query contracts, watcher contracts, shell contracts, and orchestration contracts. This split allows the UI to stay thin while index and query logic evolve independently.

Data flow follows a predictable loop: bootstrap scope index, accept input, produce ranked candidates, render the newest request only, execute file action, and continue applying filesystem deltas in the background. That separation between background ingestion and foreground query is the main reason responsiveness remains stable as dataset size grows.

## Advantages and tradeoffs

The primary advantage is interaction quality. The app feels closer to a launcher than a traditional filesystem browser because it prioritizes rapid narrowing and action. Another advantage is operational transparency, since scope, tracking state, and delta counters are visible directly in the UI.

The tradeoff is scope by design. This project is not trying to be a full replacement for enterprise content search, document semantic indexing, or policy-heavy retrieval workflows. It is optimized for fast local file discovery and immediate action.

## Running the project

Install Rust and MSVC build tools, then run from the repository root where `Cargo.toml` exists.

```bash
cargo check --workspace
cargo run -p wizmini-native
```

If you are in a different directory, pass the manifest explicitly.

```bash
cargo run -p wizmini-native --manifest-path D:\Programing\WizMiini\FunFixForWindowsSearch\Cargo.toml
```

## Current status and direction

The app already provides a functional native prototype with tray integration, global hotkey behavior, scope-aware indexing, command workflow, and responsive search UX. Ongoing work continues to harden indexing internals, improve recovery and checkpointing, and further reduce latency variance at larger scale.

If you want a deeper technical walkthrough of the design rationale, read `post.md` and `docs/architecture.md`.
