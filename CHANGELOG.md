# Changelog

All notable changes to this project are documented in this file.

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
