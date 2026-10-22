# ADR 0035: Hot-journal recovery probes RESERVED and shares one fd with `Pager`

> Moved from t-rust-db/tools `sqlite-rs/.openspec/adr/0024-hot-journal-recovery-reserved-probe-single-fd.md` (#575). Paths are db-core's; `sqlite-rs:` paths are in t-rust-db/tools' `sqlite-rs/`.

**Status:** Accepted · **Date:** 2026-08-22

## Context

#172/ADR-0032 made `Pager::open` recover a hot journal automatically based
on the journal's header magic alone — no lock-state check. That was
correct for V1/V3 scope (003-pager's Requirements 1/6 explicitly deferred
locking to #45), but #359 (V5 Slim: `.openspec/plan.md`'s "Pager: ... all
5 lock states") re-scoped hot-journal recovery to be race-safe against a
second, live connection.

Stock SQLite (`os_unix.c`'s `hasHotJournal`/`sqlite3PagerSharedLock`,
`pager.c`) never treats a journal as hot purely from its magic: it also
non-blocking-probes whether some other connection holds RESERVED (or
higher). If so, that connection is either mid-transaction or already
rolling the same journal back itself — recovering here too would race it.
Only once the probe is clear does SQLite escalate its lock, and it jumps
straight from SHARED to EXCLUSIVE, deliberately *skipping* RESERVED: taking
RESERVED first would let a third opener observe RESERVED and wrongly
conclude "someone else already validated this, safe to read" while
recovery is still in flight.

Wiring this in exposed a second, independent problem: `recover_hot_journal`
opened its own fresh `vfs.open_write(db_path)` handle, separate from the
fd `WritablePageSource`/`lock_shared()` opened moments later for the same
path. POSIX `fcntl` record locks are scoped to `(process, inode)`, not the
open file description — closing *either* fd drops the lock for both. #45
had already flagged this exact "`close()` drops all `fcntl` locks on the
inode" trap and explicitly deferred a fix, on the grounds that "nothing in
this crate yet opens two fds to the same path." Adding the RESERVED probe
meant that had quietly become false.

## Decision

- `src/storage/row/vfs/lock.rs`'s `FileLockState` (#357's 5-state ladder, previously
  wired into nothing outside its own unit tests) now backs
  `UnixVfsFile::lock_shared` directly, via a `Rc<RefCell<FileLockState>>`
  shared between the file's I/O and any lock guard it hands out. Dropping
  a guard only releases the lock level it represents; the fd itself stays
  open as long as any `Rc` clone (the `UnixVfsFile` or a live guard) is —
  so there is exactly one fd per open file for its whole lifetime, matching
  `sqlite3PagerSharedLock`'s own single-`pFd` design.
- `FileLockState` gained `check_reserved()` (`fcntl(F_GETLK)` on
  `RESERVED_BYTE` — a query, not an acquisition) and the lock-guard
  contract (`SharedLockGuard`, previously an empty marker trait) gained
  `check_reserved`/`escalate_to_exclusive`/`de_escalate_to_shared`, each
  defaulted to a no-op so `MemoryVfs`'s guard needs no changes.
- `Pager::open` now opens the main db file exactly once, acquires SHARED,
  and — only if the journal's magic was already confirmed hot — probes
  RESERVED, fails with `VfsError::Locked` if held, otherwise escalates
  straight to EXCLUSIVE, recovers using that same handle
  (`recover_hot_journal` no longer opens a file itself), downgrades back
  to SHARED, and hands that one handle to a new
  `WritablePageSource::from_file` constructor. `WritablePageSource::open`
  (a fresh `vfs.open_write` + separate `lock_shared()`) remains for every
  caller that has no hot-journal recovery to interleave.

## Alternatives rejected

- **Add the RESERVED probe without consolidating to one fd** (keep
  `recover_hot_journal`'s own `open_write` call, probe/lock through a
  second handle). Rejected: this is the exact multi-fd trap #45 already
  named and deferred — probing or holding a lock on one fd while I/O
  happens through a different, independently-closable fd to the same path
  is not just untidy, it is actively unsound under POSIX `fcntl` semantics.
- **Take RESERVED before EXCLUSIVE during recovery** (the "normal" ladder
  step order `FileLockState::set_level` already supports for ordinary
  writers). Rejected: this is precisely what stock SQLite avoids on this
  path, per `sqlite3PagerSharedLock`'s own comment — a racing opener must
  never observe RESERVED held for a reason other than "an ordinary writer
  is active," or it will misinterpret recovery-in-progress as "safe to
  read now."
- **Retry/busy-loop internally on a RESERVED conflict** instead of
  returning `VfsError::Locked` immediately. Rejected as out of scope here:
  this crate has no busy-handler/`busy_timeout` machinery yet (that is
  V5's "Busy handler, `busy_timeout`, deadlock-avoiding lock upgrade
  rules" line in `.openspec/plan.md`, a separate ticket) — returning the
  existing `Locked` error is the correct primitive for a future retry loop
  to build on, not a design dead end.

## Consequences

- `src/storage/row/vfs/lock.rs`'s standalone `lock_shared`/`UnixSharedLock` (a
  simpler, non-ladder SHARED-only lock, #45/#50's original implementation)
  is now redundant with `FileLockState` and was removed; its two direct
  unit tests were rewritten against `FileLockState` instead, preserving
  the same coverage.
- 003-pager's "Locking is out of scope" framing is stale for Requirement 1
  now that `Pager::open` takes real locks — the spec's own text has been
  updated to point at this ADR and #357/#359 rather than rewritten
  wholesale, since Requirement 6 already documents recovery's mechanics in
  detail.
- Any future caller needing a second concurrent handle to an
  already-locked path (the fd-cache scenario #45 deferred) still needs
  that per-inode cache — this ADR does not add one, it only avoids needing
  one for hot-journal recovery specifically.
