# Changelog

All notable changes to this project are documented in this file.

## 2026-02-26

### Added
- Added a new standalone `RustSearch` app (`RustSearch/`) as an `egui` + `egui_ratatui` + `ratatui` implementation of the native launcher workflow.
- Added RustSearch command parity for scope, indexing, and utility directives, including `/entire`, `/all`, `/x:`, `/up`, `/track`, `/latest`, `/last`, `/reindex`, `/testProgress`, `/fullscreen`, `/fullheight`, and `/exit`.
- Added RustSearch tray + global hotkey integration and startup privilege notice overlay with centered ASCII-art warning presentation.
- Added RustSearch implementation docs (`RustSearch/README.md`, `RustSearch/REBUILD_PLAN.md`, `RustSearch/IMPLEMENTATION_REPORT.md`).

### Changed
- Changed RustSearch window behavior to use top slide in/out motion, borderless viewport styling, centered width positioning, and visible-by-default startup.
- Changed RustSearch command menu to render as a true popup overlay (separate background) without pushing results layout.
- Changed RustSearch progress phase labels to user-facing terms (`reading snapshot`, `reading index`, `finalizing index`, `live updates`, `ready`) and improved progress label contrast.

### Fixed
- Fixed RustSearch result navigation so selection movement scrolls through off-screen rows.
- Fixed RustSearch notice overlays to dismiss on first keypress and preserve readable spacing around content.
- Fixed RustSearch command execution flow so slash-command Enter clears the query prompt while normal search Enter behavior remains unchanged.

## 2026-02-23

### Changed
- Increased selected-row contrast in both search results and command suggestions with a stronger background and border.
- Updated selected-row border shape to square corners for a sharper terminal-like look.
- Refactored native app structure by splitting large `main.rs` responsibilities into focused modules: `ui`, `update`, `windowing`, `indexing`, `indexing_ntfs`, `search_worker`, and `storage`.
- Moved search execution to a dedicated worker thread with explicit cancellation and progress events to keep UI event handling responsive.
- Added non-blocking search progress display using the same progress panel style used for indexing phases.

### Fixed
- Fixed Quick Start Enter behavior so Enter confirms the selected Quick Start action instead of opening files behind the overlay.
- Fixed keyboard navigation focus drift so typing `/` still works immediately after Arrow/Page/Home/End navigation.
- Fixed non-elevated warning overlay dismissal so it now hides on any key press (including arrow keys).
- Fixed startup responsiveness regression by removing blocking snapshot restore from initial UI render path.
- Fixed search input focus restoration after Quick Start close actions and after index completion when panel is visible.
- Fixed repeated search restarts during live indexing by removing immediate delta-triggered re-search and relying on coalesced refresh conditions.
- Fixed unexpected background searches by limiting automatic refresh scheduling to empty-query or latest-only modes.

## 2026-02-21

### Added
- Added first-run Quick Start overlay with practical keyboard guidance and command examples.
- Added persistent `Don't show again` preference for Quick Start (`%LOCALAPPDATA%\\WizMini\\quick-help-dismissed.txt`).
- Added keyboard paging/navigation handlers for results and command lists: `PageUp`, `PageDown`, `Home`, and `End`.
- Added bundled Consolas font assets for consistent app typography (`apps/native/assets/fonts/consola.ttf`, `consolab.ttf`).

### Changed
- Changed selection indicator from plain `>` to a larger, high-contrast arrow marker for better visibility.
- Changed marker rendering to use symbol-friendly font settings and vertical nudge alignment for clearer row targeting.
- Changed indexing progress copy to distinguish refresh behavior from full rebuild behavior (`Updating index` vs `Building full index`).
- Changed startup behavior to open the panel automatically when Quick Start is visible.
- Changed top-layout composition to remove empty spacer containers and tighten spacing under the search input.

### Fixed
- Fixed Quick Start interaction flow by adding keyboard-first action selection and confirmation logic.
- Fixed an excessive visual gap between the search prompt and status region when command/index panels are hidden.

## 2026-02-20

### Added
- Added `/up` command to relaunch the app elevated on Windows.
- Added `/track` command to toggle live event tracking on/off.
- Added `/latest [window]` command to filter by recent USN-timestamped file changes.
- Added `/last [window]` alias for `/latest`.
- Added `/reindex` command to force reindexing of the current scope.
- Added status counters for live index deltas since last full index (`+added ~updated -deleted`).
- Added in-memory index usage estimate to the scope/status line.
- Added lightweight in-memory filename accelerator (exact + short-prefix index).
- Added command/search helper modules (`apps/native/src/commands.rs`, `apps/native/src/search.rs`).
- Added binary scope snapshot persistence for faster warm loads (`%LOCALAPPDATA%\\WizMini\\snapshots\\scope-*.bin`).

### Changed
- Switched indexing to full-drive coverage (removed previous file-count cap).
- Updated progress reporting to show real phase progress (`Indexing` and `Building index map`).
- Changed non-elevated startup behavior to begin from current-folder scope with dirwalk fallback.
- Improved command handling so unknown slash commands do not accidentally open selected files.
- Improved query responsiveness with stronger debounce and incremental search processing.
- Changed `/latest` filtering to use tracked recent event timestamps for better live delta correlation.
- Changed tracking behavior so `/latest` and `/last` command visibility depends on `/track` state.
- Reduced memory pressure by storing path-first entries and deriving filename on demand.
- Changed typing behavior to cancel active search on keypress and defer new search until typing pauses.
- Changed snapshot serialization from JSON to binary (`bincode`) to reduce size and improve load/save performance.
- Changed UX so search input is disabled while indexing/progress is active.

### Fixed
- Fixed `/up` startup scope regression by forwarding explicit startup scope args.
- Fixed repeated/double indexing triggers for the same active scope.
- Fixed command-enter routing where slash commands could be ignored in command mode.
- Fixed privilege warning presentation with a real list-area overlay and clear dismissal behavior.
- Fixed `/last` command execution edge case where Enter could open a file instead of applying command.
- Fixed input stalls caused by synchronous filename-index construction by switching to incremental build steps.
- Removed stale dead-code warnings from unused snapshot helper functions.
