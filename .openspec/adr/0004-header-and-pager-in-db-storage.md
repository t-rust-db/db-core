# ADR 0004: Database header and pager are `db-storage::row` modules

## Status

Accepted. Location is governed by ADR 0006.

## Decision

The on-disk page layer of the row engine -- the 100-byte database
header (`db_storage::row::header`) and the pager with its WAL, rollback
journal, freelist and checkpointing (`db_storage::row::pager`) -- lives
in `db-storage`, not in `db-core`.

- `header` is a self-contained module. Besides the header parse/build
  it holds the two mode enums `JournalMode` and `SynchronousMode`, so
  that `PRAGMA` handling can name them without depending on the pager.
- `pager` depends on `row::vfs` (ADR 0003) and `row::header`, nothing
  else. `PageSource` is a local trait implemented directly for
  `RefCell<Pager>`, which is how a VM sharing one pager across cursors
  and transaction hooks reaches pages.
- Errors stay per module (`PagerError`, `WalError`, `JournalError`,
  `FreelistError`), composed by wrapping (ADR 0001).

## Consequences

- `db-core` compiles no page-level code and has no dependency on
  `db-storage` (ADR 0006, ADR 0008). The row VM sees pages only through
  a consumer-supplied cursor adapter.
- Crash-safety fixtures (hot-journal recovery, auto-vacuum pointer maps)
  are `db-storage`'s tests, run against real `sqlite3`-written files.
