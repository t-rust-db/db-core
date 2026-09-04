# ADR 0004: `sql-pager` extraction, and the `sql-header` dependency it forced

> Source: `#15` — extract sqlite-rs's `src/pager/*` (Phase 2)

## Status

Accepted. `sql-header` (`crate::header`, 498 lines) and `sql-pager`
(`crate::pager`, 4,613 lines) both extracted verbatim into new `db-core`
crates, built against `sql-vfs` (`ADR 0003`) and each other.

**Superseded (location only) by [ADR 0006](0006-storage-consolidation-into-db-storage.md), `#39`:** both `sql-header` and `sql-pager` have since moved out of `db-core` into `db-storage`'s `row` module (`db_storage::row::header`, `db_storage::row::pager`). The `SharedPager` newtype documented below no longer exists post-move — `PageSource` became a local trait once `vfs`/`pager` were merged into submodules of the same crate, so the orphan-rule workaround it existed for is no longer needed (`impl PageSource for RefCell<Pager>` directly, as originally attempted). Likewise the `sql-vfs` `test-util` feature this ADR introduced was removed post-move for the same reason (`#[cfg(test)]` crosses module boundaries within one crate). Everything else here — the extraction rationale, the `mod fixtures` deferral, the fixture-file selection — still holds unchanged.

## Context

`#15` scoped the extraction to `src/pager.rs` + `src/pager/*`
(`checkpoint.rs`, `error.rs`, `freelist.rs`, `journal.rs`, `wal.rs`) — the
issue's own "2,082 lines" estimate was stale; the actual current total is
4,613 lines. Its production code depends on exactly two things outside
itself: `crate::vfs::*` (already reconciled as `sql-vfs`, `ADR 0003`) and
`crate::header::{JournalMode, SynchronousMode}` — two small, dependency-free
enums that live in `header.rs` only so `vdbe/pragma.rs` (a future,
unextracted phase) can name them without importing `crate::pager`
directly, a layering rule `sqlite-rs`'s own `ADR-0036` established. `crate
::header` itself is 498 lines, self-contained (its only dependency is
`crate::record::TextEncoding`, already public in `sql-record`, `#13`), and
has zero coupling to `btree`/`vdbe` production code.

Extracting `sql-pager` without `sql-header` would mean either duplicating
`JournalMode`/`SynchronousMode` (reintroducing exactly the layering
problem `ADR-0036` solved, the moment a future `vdbe` extraction needs the
same enums pager also needs) or leaving pager's real, extensive
production use of them (`Pager` struct fields, `set_journal_mode`/
`synchronous`/`set_synchronous` public API, `journal_mode_from_page1`)
unresolved. So `header.rs` extracts too, ahead of `pager.rs`, as its own
crate, `sql-header` — confirmed to have zero `unsafe` and no
`btree`/`vdbe` coupling, same as `sql-vfs` and `sql-record` before it.

Both `PagerError` and its three siblings (`WalError`, `JournalError`,
`FreelistError`) move unchanged in shape, per `#15`'s explicit
instruction not to speculatively centralize them into `sql-error`
(`sql-parser::Span`'s crate — holds only `Span`, nothing else, decided
earlier this session). `PagerError` and `JournalError` directly wrap
`sql-vfs::VfsError`; `FreelistError` has no VFS dependency at all.

## Decisions made during the move

- **`sql-header` is a new, separate crate**, not folded into `sql-pager`
  — `crate::header::DatabaseHeader` (the 100-byte header parse/build,
  not just the two mode enums) is a much broader shared type future
  phases (`btree`, `vdbe`, `dump`, `integrity`, `planner`, `schema` all
  import it in sqlite-rs today) will need directly, independent of
  `sql-pager`.
- **The `mod fixtures` sub-block inside `pager.rs`'s own `#[cfg(test)]
  mod tests`** (`hot_journal_fixture_recovers_committed_state`,
  `table_single_page_fixture_reads_identically_through_pager`,
  `autovacuum_fixture_reads_identically_through_pager`, and siblings) is
  **not** ported — it exercises `Pager` together with
  `crate::btree::TableCursor`, `crate::schema::read_schema`, and
  `crate::record::{decode_record, Value}`, none of which exist in
  `db-core` yet. These are genuine, valuable crash-safety integration
  tests (hot-journal recovery against a real `sqlite3`-written fixture,
  auto-vacuum pointer-map traversal through `Pager`) — they stay in
  sqlite-rs's own `pager.rs` until `btree`/`schema`/`record` extraction
  phases land, at which point they can move as a set. All of pager's
  *other* tests (78, covering `PageCache`, WAL, journal, freelist,
  locking) moved and pass unchanged.
- **`impl PageSource for RefCell<Pager>` doesn't survive the crate
  split** — `PageSource` (`sql-vfs`) and `RefCell` (`std`) are both
  foreign to `sql-pager`, and `RefCell` isn't one of the few
  orphan-rule-exempt "fundamental" wrapper types, so this specific impl
  is now rejected as an orphan impl regardless of `Pager` itself being
  local. Replaced with `SharedPager`, a local newtype wrapping
  `RefCell<Pager>` with `new`/`borrow`/`borrow_mut`, implementing
  `PageSource` on the newtype instead. Behavior is identical; the only
  consumer (`vdbe::Vm::with_writable_db`, #194) doesn't exist in
  `db-core` yet, so this is a forced-by-crate-splitting adaptation with
  no current caller to break, not a functional change.
- **Several `pub(crate)` items promoted to `pub`**, extending the same
  reasoning `ADR 0003` already established for `sql-vfs` (items reachable
  only from the not-yet-moved `pager`/`vdbe` sibling modules, now
  needing cross-crate visibility): `Pager::tx_lock_level` (`sql-pager`,
  consumed by `vdbe/control.rs` in sqlite-rs today), and in `sql-vfs`:
  `wal_read_lock_byte` (used directly in `sql-pager`'s own
  `checkpoint.rs` tests; also genuinely used in `sql-vfs`'s own
  production code, so promoted unconditionally, not gated).
- **`#[cfg(test)]` doesn't cross a crate boundary — a new `test-util`
  Cargo feature on `sql-vfs` was required.** `sql-pager`'s tests need
  `sql-vfs`'s cross-process lock-contention test helpers
  (`test_lock_probe` module and its `lock_available`/
  `lock_held_by_subprocess`/`hold_multiple`/`release_all`, plus
  `lock::exclusive_lock_available`, `lock::reserved_byte_range`,
  `shm::slot_is_free_test_only`) to observe `Pager` lock state from
  outside `sql-vfs` — but these were all `#[cfg(test)]`-gated, which
  (unlike `pub`/`pub(crate)`) is never visible to a *different* crate's
  test build, no matter the visibility modifier. Regated to
  `#[cfg(any(test, feature = "test-util"))]`; `sql-pager`'s
  `[dev-dependencies]` enables `sql-vfs`'s `test-util` feature. Not
  a default feature — these exist purely for tests.
- **Found and fixed while extracting**: `cargo test -p sql-vfs` alone
  never builds `sql-vfs`'s own `src/bin/lock_probe.rs` (`cargo test`
  doesn't build sibling `bin` targets unless something forces it) —
  meaning `#31`'s (`ADR 0003`) merged test suite only ever passed
  locally because `--bins` had been built manually first in that
  session, not under a plain `cargo test`/`make test`. `sqlite-rs`'s own
  `Makefile` already solved this (`cargo build --bin lock_probe` before
  `cargo test`) — `db-core`'s `Makefile`'s `test` target now does the
  same, since `sql-pager`'s tests need the same binary via the
  `test-util` dev-dependency.
- **Only the fixture `.db`/`.db-wal` files each crate's tests actually
  read moved** (`sql-header/tests/corpus/fixtures/{pagesizes,encodings,
  invalid}/*`, `sql-pager/tests/corpus/fixtures/journalstates/*.db-wal`)
  — not sqlite-rs's full shared `tests/corpus/fixtures/` tree, which
  many still-unextracted modules also read from.
- **Found and fixed while extracting, unrelated to this issue's own
  scope**: `sql-record`/`sql-header` both correctly carry the
  `[lints.clippy]` baseline matching sqlite-rs's own (`unwrap_used`,
  `expect_used`, `indexing_slicing`, `panic`, `arithmetic_side_effects`
  denied) — `sql-vfs` (`#14`/`ADR 0003`) was missing it despite its own
  code assuming the baseline is active (`#[allow(clippy::unwrap_used,
  ...)]` throughout its test modules, which are dead `allow`s without
  the corresponding `deny`). Added `[lints.clippy]` to `sql-vfs`'s
  `Cargo.toml` to match, while already touching that crate for the
  `test-util` feature.

## Consequences

- Workspace members gain `sql-header` and `sql-pager`; both build
  against `sql-vfs`'s and each other's public API with no visibility
  changes needed beyond what's listed above.
- `sql-header`: 16/16 tests pass. `sql-pager`: 78/78 tests pass
  (`mod fixtures`'s ~9 cross-module integration tests excepted, per
  above). `sql-vfs`: 49/49 tests pass (unchanged in count, now provable
  under a plain `cargo test`/`make test` rather than requiring a manual
  `--bins` build first).
- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt
  --check` clean across the whole workspace.
- sqlite-rs's own `src/header.rs`/`src/pager/*` are **not** deleted or
  repointed at `sql-header`/`sql-pager` by this ADR — same deferral
  `#11`/`#14` made for `sql-sys`/`sql-vfs`. Tracked as a follow-up in
  that repo.
- Follow-up, explicitly not resolved here: once `btree`/`schema`/
  `record`'s own extraction phases land in `db-core`, `pager.rs`'s
  `mod fixtures` integration tests (hot-journal recovery, autovacuum
  pointer-map traversal through `Pager`) should move here as a set —
  they're real crash-safety coverage, not incidental to skip. Re-check
  this ADR's "not ported" list against `db-core`'s crate set at that
  point rather than assuming it's still accurate.
- `#15`'s own "Blocks: `sql-btree` (next ticket)" note stands: `sql-btree`
  now has both `sql-vfs` and `sql-pager` to build against.
