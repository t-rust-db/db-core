# ADR 0041: `PRAGMA synchronous` fsync-skip policy, and why `SynchronousMode` lives in `header.rs`

> Moved from t-rust-db/tools `sqlite-rs/.openspec/adr/0036-pragma-synchronous-fsync-policy.md` (#575). Paths are db-core's; `sqlite-rs:` paths are in t-rust-db/tools' `sqlite-rs/`.

Date: 2026-08-29

## Context

#645: `PRAGMA synchronous` was silently ignored — every commit
unconditionally did the fsyncs that only match stock SQLite's `FULL`
(the default), with no way to opt into `NORMAL`/`OFF`. Implementing it
for real means deciding, precisely, which of `Pager`'s three
commit-time fsyncs each level skips:

- The rollback-journal fsync (`flush_locked`, before writing any dirty
  page into the main file) — the safety ordering `recover_hot_journal`
  depends on to replay a hot journal after a crash mid-write.
- The rollback main-file fsync (`flush_locked`, after writing every
  dirty page) — the second half of the two-fsync rollback-journal
  commit protocol.
- The WAL frame fsync (`flush_wal_locked`, before publishing the new
  `mxFrame`) — WAL mode's own single commit-time fsync.

Stock SQLite's documentation gives a clear, if slightly asymmetric,
answer: `NORMAL` still fsyncs "at the most critical moments" in
rollback-journal mode, but in WAL mode it only syncs at checkpoint
boundaries, not on every commit. `OFF` never fsyncs at all. Chasing
every historical nuance of `getSafetyLevel`'s masking of arbitrary
out-of-range integer values, `EXTRA`, and the `ON` boolean alias was
explicitly out of scope (#645's acceptance criteria only names
`FULL`/`NORMAL`/`OFF`).

A second, unrelated question came up during implementation: `vdbe/`
may never `use crate::pager` directly (sqlite-rs spec 001-architecture
Requirement 1, "VDBE does not know file format", enforced by
`sqlite-rs:tests/unit/layer_isolation.rs`). `synchronous`'s state is exactly the
kind of thing `vdbe/pragma.rs` needs to name directly (to convert
between the wire-level `i32` opcode operand and a real enum, and to
report the current value back as a query result) — the same situation
`JournalMode` was already in, and already solved by living in
`src/storage/row/header.rs` rather than `src/storage/row/pager.rs`.

## Decision

**Fsync policy** (`Pager::flush_locked`/`flush_wal_locked`):

| Level    | Journal fsync (rollback) | Main-file fsync (rollback) | WAL frame fsync |
|----------|:---:|:---:|:---:|
| `Full`   | yes | yes | yes |
| `Normal` | yes | no  | no  |
| `Off`    | no  | no  | no  |

`Normal` keeps the rollback-journal fsync because it's what makes
`recover_hot_journal` safe at all — without it, a crash between the
journal write and the main-file write could leave a journal on disk
whose own bytes never made it to a stable state, defeating the
recovery it exists to enable. It drops the main-file fsync (the thing
`FULL` adds on top) and the WAL frame fsync, matching stock SQLite's
documented "still consistent, less durable" `NORMAL` semantics for
both journal modes.

Two other fsync call sites are deliberately left ungated:
`Pager::set_journal_mode`'s own page-1 write-back (a mode switch, not
part of any user transaction) and `checkpoint::checkpoint_passive`'s
post-backfill fsync (checkpoints sync the main file under `NORMAL`
too, per stock SQLite's docs — gating it would need threading
`synchronous` through a free function that doesn't otherwise touch
`Pager` state, for a case `#645`'s acceptance criteria doesn't ask
for).

**Where `SynchronousMode` lives**: `src/storage/row/header.rs`, next to
`JournalMode`, even though — unlike `JournalMode` — it has no on-disk
representation at all (stock SQLite never persists `synchronous`; a
fresh connection always starts at `Full`). `header.rs` is the
established "vdbe-safe vocabulary" module for exactly this situation:
an enum `vdbe/pragma.rs` must reference by name (to build/read it) but
that `src/storage/row/pager.rs` itself would otherwise be the natural home for.

## Alternatives rejected

- **Cache the fsync policy as three precomputed booleans** (`sync_journal`,
  `sync_main_file`, `sync_wal_frame`) instead of a `SynchronousMode`
  enum matched at each call site. Rejected: the enum is the thing
  `PRAGMA synchronous` (query form) needs to report back verbatim
  (`0`/`1`/`2`), and three booleans would just be a less legible
  re-encoding of the same three-level table above, computed twice.
- **Also gate `checkpoint_passive`'s fsync and the mode-switch page-1
  write-back** on `synchronous`. Deferred: neither is a per-commit hot
  path, `#645`'s acceptance criteria only asks about "the correct
  fsync call pattern" for commits, and stock SQLite's own checkpoint
  fsync isn't skipped by `NORMAL` anyway — only `OFF` would change
  anything there, a narrower case not worth the extra plumbing yet.
- **A general per-connection settings struct** (rather than fields
  directly on `Pager`, mirroring `journal_mode`) to hold `synchronous`
  and future PRAGMA-set state. Rejected as premature: there is
  currently exactly one other stateful PRAGMA (`journal_mode`), it
  already lives as a bare `Pager` field, and introducing a wrapper
  struct for two fields is speculative until a third stateful PRAGMA
  actually arrives.
- **`Pager::set_synchronous` re-checking for a pending transaction**
  (mirroring `set_journal_mode`'s `PendingTransaction` guard). Rejected:
  stock SQLite explicitly allows changing `synchronous` mid-transaction
  (it only affects fsync behavior at the *next* commit, unlike
  `journal_mode`, which needs a clean transaction boundary to safely
  rewrite page 1's version bytes and swap journal implementations).

## Consequences

- `Pager::synchronous`/`Pager::set_synchronous` are the only two new
  public `Pager` methods; no existing call site changes shape.
- `vdbe/pragma.rs::synchronous` is the first pragma executor to both
  set state (mirroring `set_journal_mode`) and emit a result row
  (mirroring `integrity_check`) from the same opcode, keyed off a
  sentinel `P1` value (`SYNCHRONOUS_QUERY = -1`) rather than a second
  opcode — kept as one opcode since the two forms share the same
  "resolve the writer, if any" prologue.
- A future PRAGMA that also needs bidirectional get/set (unlike
  `journal_mode`'s still-write-only shape) has a second precedent to
  follow beyond `integrity_check`'s read-only one.
- `src/storage/row/vfs/memory.rs`'s `MemoryVfs` gained a `sync_calls()` counter
  (shared across clones and every file handle opened from it) purely
  so `src/storage/row/pager.rs`'s new fsync-gating tests can assert *whether* a
  commit fsynced — an in-memory backend has no other way to observe
  that.
