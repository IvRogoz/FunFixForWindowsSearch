# Changelog

All notable changes to this project are documented in this file.

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
