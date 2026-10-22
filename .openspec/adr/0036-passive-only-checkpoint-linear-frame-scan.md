# ADR 0036: PASSIVE-only checkpoint, single non-blocking lock, linear frame scan

> Moved from t-rust-db/tools `sqlite-rs/.openspec/adr/0025-passive-only-checkpoint-linear-frame-scan.md` (#575). Paths are db-core's; `sqlite-rs:` paths are in t-rust-db/tools' `sqlite-rs/`.

Date: 2026-08-23
Status: Accepted

## Context

#386 (V6.2, epic #354) needed a WAL checkpoint that a `Pager` can run to
copy committed WAL frames into the main database file. SQLite itself
supports four checkpoint modes (PASSIVE, FULL, RESTART, TRUNCATE) and
resolves each committed page's frame via an in-memory hash table built
from the wal-index (`wal.c`'s `walIndexPage`), avoiding a full linear scan
of the WAL on every checkpoint.

Building either of those in full — multi-mode checkpoint coordination
with the write-lock, or the O(1) hash-table lookup — is more than #386's
scope: the epic's own Out of Scope table defers `journal_mode=WAL`
switching to #388 and multi-reader/writer concurrency to #389, and this
repo's WAL reader (`wal::committed_pages`, from #383) already resolves
pages via a linear frame scan for the read path, so a checkpoint built on
top of it inherits that same scan for free rather than needing its own
lookup structure.

## Decision

**Implement PASSIVE only, coordinated by a single non-blocking
`WAL_CKPT_LOCK`.** `checkpoint_passive` (`src/storage/row/pager/checkpoint.rs`) never
waits for a reader to finish — it bounds its own progress by the minimum
currently-active reader mark (`src/storage/row/vfs/shm.rs`'s `active_reader_marks`)
and copies only the frames up to that mark. Two concurrent checkpoint
attempts are serialized by `WAL_CKPT_LOCK` (`WAL_CKPT_LOCK_BYTE`,
`UNIX_SHM_BASE + 1`, matching `wal.c`'s real lock layout) rather than
racing on `nBackfill`. FULL, RESTART, and TRUNCATE — which additionally
require the write lock and/or blocking on readers — are deferred to V7.

**Resolve frames via `wal::committed_pages` against a truncated byte
slice, not a page→frame hash table.** `checkpoint_passive` reads the
whole `-wal` file, truncates it to exactly the safe-frame boundary
(always a commit boundary — see the module's own doc comment), and
re-runs the existing linear-scan resolver over that slice. This is a
pure performance trade-off: correctness doesn't depend on it, and the
same resolver is already exercised and trusted by the read path.

## Alternatives rejected

- **Block until every reader releases (FULL/RESTART) as part of #386**:
  rejected — requires coordinating the write lock and reader-release
  signaling that #388/#389 haven't landed yet; would have expanded #386
  well past its estimate to build machinery with no caller until a later
  ticket needs it.
- **Build the page→frame hash table now**: rejected — it's a pure
  performance optimization over `committed_pages`' existing linear scan,
  not a correctness requirement, and the read path already accepts the
  same scan cost. Building it here would spend #386's budget on
  infrastructure the checkpoint path doesn't uniquely need.
- **Take the WAL write lock during PASSIVE checkpoint** (belt-and-braces
  against a concurrent writer): rejected for now — `WAL_CKPT_LOCK` alone
  is what stock SQLite's own PASSIVE mode takes; write-lock coordination
  is part of the write-side wal-index wiring explicitly tracked at
  #388/#389, not a gap specific to checkpointing.

## Consequences

- A checkpoint pass's cost is O(WAL size) per call, dominated by reading
  the whole `-wal` file into memory and a linear scan — acceptable for
  the workloads this repo currently supports (no long-lived WAL under
  sustained concurrent write load yet), but will need revisiting if a
  large-WAL workload's checkpoint latency becomes a measured problem.
- FULL/RESTART/TRUNCATE checkpoint modes remain unimplemented; any caller
  needing "wait for all readers, then reclaim WAL space entirely" must
  wait for V7.
- The page→frame hash table remains a well-scoped, independently
  measurable follow-up if `checkpoint_passive`'s linear scan ever shows
  up as a real bottleneck — no design decision here blocks adding it
  later.
