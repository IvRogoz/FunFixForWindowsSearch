# Changelog

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
