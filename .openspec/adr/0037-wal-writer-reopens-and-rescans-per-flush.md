# ADR 0037: WAL writer reopens and rescans the `-wal` file on every flush

> Moved from t-rust-db/tools `sqlite-rs/.openspec/adr/0026-wal-writer-reopens-and-rescans-per-flush.md` (#575). Paths are db-core's; `sqlite-rs:` paths are in t-rust-db/tools' `sqlite-rs/`.

Date: 2026-08-23
Status: Accepted

## Context

#389 (V6.3, epic #354) makes `Pager::flush` actually write through the WAL
once `journal_mode=WAL` is active, instead of always falling through to
the rollback-journal path (#388 could only switch modes, not write
frames). Each commit must append its dirty pages as new frames onto
whatever the `-wal` file already contains — which, for the second and
later commits from the same `Pager`, means frames from earlier flushes,
and for a `Pager` opened directly against an already-active WAL database,
means frames from some other connection entirely.

`WalWriter::create` (#386) always starts a writer at a fresh header with
zero frames — reusing it for a second commit would silently overwrite
frame 1 and desynchronize the running checksum chain `committed_pages`
verifies on read. Something has to determine "where do valid frames
currently end, and what's the checksum state there" before a second
commit can safely resume.

## Decision

**`WalWriter::open_existing` resumes by re-reading and rescanning the
whole `-wal` file on every call, rather than `Pager` caching one
persistent `WalWriter` across flushes.** `flush_wal_locked` opens a fresh
`WalWriter::open_existing` each time it runs: reads the file, parses the
header, and walks every valid frame (`last_valid_frame_state`, the same
salt/checksum validity rules as `committed_pages`) to recover the byte
offset and running checksum to resume from. `mxFrame` in `-shm` is
published (`shm::publish_mx_frame`) with a plain `pwrite`, the same
best-effort, non-atomic pattern already accepted for `nBackfill`
(`publish_backfill`, #386) — no compare-and-swap or additional locking
beyond `WAL_WRITE_LOCK` already serializing writers.

The writer's own post-commit reads are served by folding the
just-appended pages from `self.dirty` into `self.wal_pages` in memory,
rather than re-claiming a new WAL reader-mark slot for itself — the
existing `wal_lock`/`wal_pages` pair is `open`-time snapshot bookkeeping
for *this connection as a reader*, not something a write needs to touch
to be visible to its own subsequent reads.

## Alternatives rejected

- **Cache one `WalWriter` handle on `Pager`, opened once and reused
  across flushes**: rejected for now — avoids the rescan, but adds real
  state-lifetime problems this ticket doesn't need to solve yet: the
  cached handle would need invalidating whenever `switch_wal_to_journal`
  deletes the `-wal` file or `switch_journal_to_wal` recreates it, and
  would need to tolerate a torn/short file from a `Drop`-order crash
  between flushes. Reopening fresh each time makes every flush correct
  and self-contained by construction, at the cost of an O(WAL size)
  rescan per commit.
- **Track `mxFrame`/append offset entirely in `Pager` state instead of
  rescanning `-shm`/`-wal`**: rejected — `Pager` doesn't durably persist
  any such counter today, and deriving it from the files themselves is
  the only way a `Pager` opened fresh against an already-active WAL
  (written by some other connection) computes the right resume point.
- **Re-claim a fresh WAL reader-mark slot after committing, so the
  writer's own reads go through the normal reader snapshot path**:
  rejected — reader-mark slots are a bounded resource (4 slots) meant for
  actual concurrent readers; churning through one on every write this
  connection makes to serve its own reads would waste them for no
  isolation benefit, since a writer trivially knows its own uncommitted-
  turned-committed pages already.

## Consequences

- A commit's cost includes reading and validating the entire existing
  `-wal` file, not just appending — acceptable for the workloads this
  repo currently targets (same trade-off ADR-0036 already accepted for
  PASSIVE checkpoint's linear scan), but will need revisiting — most
  likely by tracking the resume offset/checksum on `Pager` across calls,
  invalidated on mode switches — if a long-lived WAL under sustained
  write load makes per-commit rescanning measurably slow.
- `mxFrame` publication has no atomicity guarantee beyond `WAL_WRITE_LOCK`
  already serializing writers; a reader claiming a mark concurrently with
  a publish can still observe a torn 4-byte value in principle, exactly
  the same known-accepted residual risk `nBackfill` already carries.
