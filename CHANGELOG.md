# Changelog

## v0.1.12 - 2026-11-27

### Fixed

- Stop idle USN polling from generating its own filesystem changes by removing unused checkpoint and per-volume snapshot writes.
- Apply live changes through persistent path lookups and incremental memory accounting instead of repeatedly scanning and copying the full index.
- Cancel superseded indexing jobs and pending snapshot writes when the scope changes or the app exits.
- Key scope snapshots by their resolved roots so different folders or drive sets cannot reuse each other's cached index.
- Refresh results after reindexing while the search panel is hidden.
- Prevent Enter or Alt+Enter from acting on stale results after query edits, including text and Enter received in the same frame.
- Serialize snapshot writers and replace complete snapshots atomically; failed, cancelled or superseded writes preserve the previous snapshot.
- Size the panel for the selected result-row count within the available work area, and restore that count after fullscreen or full-height mode.
- Show file-open and reveal failures in the status message.
- Remove ordinary deleted files by direct lookup, avoiding a full volume-tree scan for every deletion.
- Keep the previous complete volume state when journal recovery is cancelled during reopening or rebuilding, and close unused handles.

### Added

- Headless live-USN regression coverage for file and folder changes, search updates, scope-boundary moves, burst changes, idle behavior, snapshots and cancellation.
- Isolated simulations for journal reset or lost history, drive disconnection/reconnection, failed rebuilds, cancellation and handle cleanup.

### Verified

- 51 regular tests passed, including 11 recovery simulations.
- The separate real-journal integration test passed, including a 128-file create/delete burst and 15 idle seconds without persistence feedback.
- The optimized Windows executable compiled with the existing Rust toolchain and compiled dependencies; no dependencies were changed.
- Rust formatting and Git whitespace checks passed.
- Recovery faults were simulated. Physical drive disconnection, real journal reset, GUI interaction, UAC and Explorer launches were not exercised.

## v0.1.11 - 2026-11-12

### Fixed

- Open search results through Windows ShellExecute instead of passing paths to `cmd`, preventing shell interpretation of file names.
- Removed incomplete search shortcuts that could omit substring, path, and Boolean matches.
- Update descendant paths when an indexed directory is renamed or moved.
- Preserve all indexed volumes and fallback results when an NTFS volume recovers.
- Resume USN tracking only when the original journal position is still available; otherwise rebuild the volume index.
- Fall back to directory walking for individual volumes that cannot use NTFS indexing, including when elevated. Mixed scopes report partial live tracking.
- Select the newest 600 `/latest` results across all matches while keeping batch storage bounded.
- Recognize `/rows:N` when executing the command.
- Make wildcard `?` match one Unicode character, including characters encoded with multiple UTF-8 bytes.

### Verified

- `rustfmt --edition 2021 --check src/main.rs`
- `git diff --check`
- 17 tests passed in a dependency-free harness; the full Cargo test suite was not run.
- `cargo build --release --locked`
- GUI and live Windows behavior were not validated.

## v0.1.10 - 2026-07-06

### Added

- Added boolean search operators with standalone `AND` and `OR` terms.
  - `invoice AND pdf` matches entries containing both terms.
  - `invoice OR receipt` matches entries containing either term.
  - `draft AND notes OR invoice AND pdf` is evaluated as `(draft AND notes) OR (invoice AND pdf)`.
- Added incomplete boolean-query handling so `AND`, `OR`, `name AND`, and `name OR` do not trigger a literal operator search.
- Added folder indexing and folder search results alongside files.
- Added a `[D]` folder marker in both GPU and soft renderer result lists.
- Added `/rows N` and `/rows:N` commands to resize the panel downward by visible result rows.

### Changed

- Live filesystem deltas now refresh the search worker corpus and current result set after file changes.
- `/up` now relaunches elevated using the current search scope instead of always switching to the current drive.
- Current-folder NTFS live indexing now opens the containing drive and filters live results back to the selected folder.
- `src/indexing_ntfs.rs` is now self-contained in the RustSearch crate instead of re-exporting source from the sibling native app.
- Search worker state was refactored to reduce argument complexity and improve cancellation/replacement readability.

### Fixed

- Fixed stale search results after live file creates, deletes, renames, or updates.
- Fixed non-empty searches not refreshing after live index changes.
- Fixed clippy warnings so `cargo clippy --all-targets --all-features -- -D warnings` passes.

### Verified

- `cargo fmt`
- `cargo test`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo build`
