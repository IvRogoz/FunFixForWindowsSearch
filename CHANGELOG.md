# Changelog

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
