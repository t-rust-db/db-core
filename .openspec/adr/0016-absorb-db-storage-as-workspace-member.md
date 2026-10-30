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
- Features: `storage-row`, `storage-column` (= `dep:memmap2`
  — the crate's only third-party dependency, optional; `dep:ruzstd` was a
  second until #700/#692),
  `storage-test-support`. `storage-row` and `storage-column` are in
  `default`.
- `[[bin]] lock_probe` (`src/storage/row/vfs/bin/lock_probe.rs`,
  `required-features = ["storage-row"]`) — test helper, second OS process.
- Test fixtures: `tests/corpus/fixtures/`.
- `unsafe_code = "deny"` crate-wide; two audited `#[allow(unsafe_code)]`
  carve-outs: `storage::column::mmap`, `storage::row::vfs::fcntl`.

## Gates

`storage` is held to db-core's full lint tier (#289): no `cast_*` allow,
no `EXEMPT` entries in `check-panic-allows`. Two designated boundaries in
`MVL_LIMIT_EXCLUDE`, by file name, not by directory:

- `storage::row::vfs` (`vfs.rs`, `page_source.rs`, `unix.rs`,
  `memory.rs`) — the open-implementor `dyn` boundary (ADR 0003), same
  standing as `vm/row/cursor*.rs`; `fcntl.rs` — one of the two audited
  `unsafe` carve-outs.
- `storage::column::parquet/*` — a zero-copy reader over the mmap; the
  lifetimes are the design. Owning the buffer instead would be a redesign,
  not a lint fix, and is not planned. `column/mmap.rs` — the other
  `unsafe` carve-out.
- `storage::stream/*` is excluded while #304/#305 build it; lifted with
  #305.

MC/DC obligation ids are basename-keyed: `storage::row::btree::{table,
index}` submodule files carry a `table_`/`index_` prefix. The
`lock_probe` test helper lives in `tests/helpers/` (as in sqlite-rs), out
of production scanning; `test_minimal_db`'s `cfg(any(test,
feature = "storage-test-support"))` region counts as test code for the
panic-allow gate.

## Amendment: no carve-out for the ZSTD decoder (#749)

`storage::parquet::compression::zstd` stays safe Rust
(`#![deny(unsafe_code)]`, and `storage::parquet` stays `forbid`). A
prototype (scoped `unsafe` in the match and literal copy loops, plus an
unchecked FSE table lookup as a ceiling probe) gained nothing end to end
and at most 5-15% of decode time (under 5% of scan CPU) even unchecked,
against an acceptance bar of 15% scan CPU. On column-shaped payloads the
safe decoder is already 0.9-1.5x libzstd's user time. The remaining gap
to DuckDB on ZSTD data is mostly not the codec (2.4x on Snappy already);
ZSTD itself adds about 23 ms wall on the 10M-row parity set. ZSTD parity
is accepted as a documented gap. Reopen only with a new decode design,
not with `unsafe` in the existing loops. Measurements: #749.

## Consequences

- Consumers (sqlite-rs, column-rs, trigrep): drop the `db-storage`
  dependency; enable `storage-row` or `storage-column` on `db-core`;
  `db_storage::` → `db_core::storage::`.
- The standalone `t-rust-db/db-storage` repo is archived.
- ADR-0040 (sqlite-rs) "first-party crates pinned by tag" applies to
  `db-core`, `db-cli`; there is no separate storage crate to pin.
