# RustSearch Rebuild Plan (egui_ratatui)

## Goal

Recreate the current `wizmini-native` app behavior in a new `RustSearch` project using `egui_ratatui` + `ratatui` (rendered inside `egui`) while preserving fast search, indexing, command mode, and Windows-specific workflow (hotkey + tray + open/reveal actions).

## Current App Review (What must be carried over)

### Core behavior

- Global hotkey toggle (backtick) and tray menu show/hide + quit.
- Floating search panel with keyboard-first navigation.
- Search scopes: current folder, current drive, all drives, explicit drive.
- Slash commands: `/entire`, `/all`, `/x:`, `/up`, `/track`, `/latest`, `/last`, `/reindex`, `/testProgress`, `/exit`.
- Enter opens file, Alt+Enter reveals in Explorer, Esc hides panel.

### Data/indexing behavior

- Initial snapshot load from disk for fast startup.
- NTFS/USN live indexing when available (elevated mode), `walkdir` fallback otherwise.
- Background search worker with cancellation + progress events.
- Filename fast-path index (exact/prefix maps) plus full scan fallback.
- Latest-changes filtering and per-path recent change bookkeeping.

### Persistence and diagnostics

- Persisted scope and quick-help dismissed flag in `%LOCALAPPDATA%\WizMini`.
- Scope snapshots serialized with `bincode`.
- Optional debug log file output when `WIZMINI_DEBUG=1`.

## Recommended Target Architecture

### Stack

- Host app: `eframe` + `egui`.
- TUI renderer: `egui_ratatui::RataguiBackend` + `ratatui::Terminal`.
- Font backend: `soft_ratatui` (start with embedded graphics unicode fonts).
- Platform integration: keep `global-hotkey`, `tray-icon`, and `windows-sys`.

### Module split

- `src/main.rs`: app boot, eframe setup, hotkey/tray setup.
- `src/app_state.rs`: persistent runtime state (ported `App` fields).
- `src/tui_view.rs`: ratatui frame draw functions (search box, list, status, overlays).
- `src/input.rs`: keyboard mapping and command execution routing.
- `src/indexing.rs`, `src/indexing_ntfs.rs`, `src/search.rs`, `src/search_worker.rs`, `src/storage.rs`, `src/commands.rs`: mostly ported from existing app with minimal logic changes.
- `src/platform.rs`: open/reveal path, elevation helpers, Windows-only glue.

## Execution Plan (Phased)

## Phase 0 - Bootstrap

1. Create standalone Cargo project in `RustSearch` (outside existing workspace first).
2. Add dependencies (`eframe`, `egui`, `egui_ratatui`, `ratatui`, `soft_ratatui`, `global-hotkey`, `tray-icon`, `walkdir`, `windows-sys`, `serde`, `bincode`).
3. Add a minimal `egui_ratatui` demo screen and verify app launches.

Deliverable: window opens and displays a ratatui block embedded in egui.

## Phase 1 - Port non-UI core logic

1. Port `SearchItem`, scopes, backend enums, constants.
2. Port `commands.rs`, `search.rs`, `search_worker.rs`, `storage.rs`.
3. Port `indexing.rs` and `indexing_ntfs.rs` with compile-gated Windows behavior.
4. Keep tests for command parsing + search matching to lock behavior.

Deliverable: indexing/search logic compiles and can be driven headless.

## Phase 2 - Build TUI in ratatui

1. Implement panel layout in ratatui:
   - prompt line,
   - command dropdown,
   - progress panel,
   - result table/list,
   - status/footer line,
   - quick-help and privilege overlays.
2. Reproduce selected row marker and file type color coding.
3. Add layout constraints for current size (roughly current 980x560 behavior).

Deliverable: UI parity in a ratatui frame rendered via `egui_ratatui`.

## Phase 3 - Input + command parity

1. Wire keyboard events from egui to state update handlers.
2. Implement command mode navigation, Enter behavior, paging keys, Home/End.
3. Implement debounced query updates and search scheduling.
4. Implement open/reveal actions and `/up` elevation flow.

Deliverable: keyboard interactions match existing app behavior.

## Phase 4 - Platform features

1. Re-enable global hotkey toggle polling in eframe update loop.
2. Re-enable tray menu polling (show/hide and quit).
3. Implement panel show/hide UX for eframe (close/hide strategy + focus restoration).

Deliverable: app behaves like resident launcher utility, not a plain foreground window.

## Phase 5 - Performance + polish

1. Validate indexing time/memory against current app.
2. Tune search batch sizes and redraw cadence.
3. Preserve snapshot warm start and live delta updates.
4. Add a small smoke benchmark script for startup, index, and search latency.

Deliverable: performance close to or better than current baseline.

## Phase 6 - Verification + packaging

1. Functional checklist pass for every command and keyboard shortcut.
2. Elevated/non-elevated scenario test.
3. Build release binary and verify tray/hotkey on clean machine.
4. Write `RustSearch/README.md` with run/build instructions.

Deliverable: releasable `RustSearch` app with migration notes.

## Risks and Mitigations

- Event-loop differences (`iced` vs `eframe`): isolate state update logic in pure functions first.
- Hotkey/tray behavior under eframe: keep polling logic explicit and test early in Phase 0/4.
- NTFS/USN complexity: port with minimal edits and guard with fallback path if journal unavailable.
- Focus management differences: treat focus restoration as dedicated task with keyboard regression checks.

## Suggested Task Breakdown (Issue-friendly)

1. Scaffold `RustSearch` Cargo app with `egui_ratatui` demo.
2. Port search/command/storage modules unchanged where possible.
3. Port indexing + NTFS live update pipeline.
4. Build ratatui layout equivalent to existing panel.
5. Implement input/update state machine parity.
6. Integrate hotkey + tray + open/reveal platform actions.
7. Add persistence and debug logging parity.
8. Run parity test checklist and tune performance.
9. Finalize docs and release build.

## Definition of Done

- All documented commands and shortcuts from current README work in `RustSearch`.
- Startup with snapshot restore works.
- NTFS live updates work in elevated mode; dirwalk fallback works when not elevated.
- Global hotkey and tray interactions work reliably.
- Search responsiveness remains interactive on large corpora.
