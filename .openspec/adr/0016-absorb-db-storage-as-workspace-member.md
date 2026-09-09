# ADR 0016: db-storage is the `storage` module of db-core

## Status

Accepted (#287, #288)

## Decision

The db-storage crate is a module of db-core: `src/storage/` (history
preserved via `git subtree`). One crate, one `Cargo.toml`, one lint bar.
No `db-storage` crate, workspace member, or git dependency exists.

## Structure

- `src/storage.rs` — module root; `storage::row`, `storage::column`
  (`storage::stream` planned, ADR 0006).
- `storage` depends on `crate::value` and `crate::schema` only.
  `parser`/`vm`/`codegen` never import `storage`.
- Features: `storage-row`, `storage-column` (= `dep:memmap2`,
  `dep:ruzstd` — the crate's only third-party dependencies, optional),
  `storage-test-support`. `storage-row` and `storage-column` are in
  `default`.
- `[[bin]] lock_probe` (`src/storage/row/vfs/bin/lock_probe.rs`,
  `required-features = ["storage-row"]`) — test helper, second OS process.
- Test fixtures: `tests/corpus/fixtures/`.
- `unsafe_code = "deny"` crate-wide; two audited `#[allow(unsafe_code)]`
  carve-outs: `storage::column::mmap`, `storage::row::vfs::fcntl`.

## Gates

- `cast_possible_truncation`/`cast_possible_wrap`/`cast_sign_loss`
  (db-core#225) — `#![allow]` on `storage` only.
- `check-mvl-limit` — `src/storage/*` in `MVL_LIMIT_EXCLUDE`;
  `src/storage.rs` scanned.
- `check-panic-allows` — `EXEMPT`: `storage/row/btree.rs`
  (`test_minimal_db`, `cfg(any(test, feature))`),
  `storage/row/vfs/bin/lock_probe.rs`.
- MC/DC obligation ids are basename-keyed: `storage::row::btree::{table,
  index}` submodule files carry a `table_`/`index_` prefix.

All four are a worklist in db-core#289.

## Consequences

- Consumers (sqlite-rs, column-rs, trigrep): drop the `db-storage`
  dependency; enable `storage-row` or `storage-column` on `db-core`;
  `db_storage::` → `db_core::storage::`.
- The standalone `t-rust-db/db-storage` repo is archived.
- ADR-0040 (sqlite-rs) "first-party crates pinned by tag" applies to
  `db-core`, `db-cli`; there is no separate storage crate to pin.
