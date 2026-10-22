# ADR 0031: `-shm` access via `pread`/`pwrite`, not `mmap`

> Moved from t-rust-db/tools `sqlite-rs/.openspec/adr/0001-shm-access-pread-not-mmap.md` (#575). Paths are db-core's; `sqlite-rs:` paths are in t-rust-db/tools' `sqlite-rs/`.

> Source: #54 — bound and harden `-shm` mmap against `SIGBUS` on a shrinking file

## Status

Accepted. Implemented in #66 (`feat: eliminate all unsafe from src/vfs,
restore crate-wide forbid(unsafe_code)`), ahead of this ADR being written —
this document records the decision retroactively so "how sqlite-rs accesses
`-shm`" has an explicit architectural rationale rather than living only in a
module-doc paragraph.

This `-shm`-specific decision was later generalized into the crate-wide
zero-unsafe policy — see [sqlite-rs ADR-0009](https://github.com/t-rust-db/tools/blob/main/sqlite-rs/.openspec/adr/0009-zero-unsafe.md).

## Context

`src/storage/row/vfs/shm.rs` claims WAL reader-mark slots by reading and writing fields
in a database's `-shm` companion file (the wal-index header: `mxFrame`,
`aReadMark[5]`, the lock-byte region). The original implementation `mmap`'d
this file. Two exposures followed, both flagged in the security review of
#53:

1. **`SIGBUS` on a shrinking `-shm` file.** The mapped length was validated
   once at `open` time. A concurrent `PRAGMA wal_checkpoint(TRUNCATE)` can
   legitimately shrink the file afterwards; an access into now-unbacked
   pages raises `SIGBUS` — an uncatchable process kill, not a Rust panic.
   This violates the crate's otherwise-consistent "never crash on
   malformed or racing input" property.
2. **No upper bound on the mapped length**, trusting `file.metadata()?.len()`
   directly from the filesystem.

## Options considered

- **A — Bound the mapping and re-validate.** `fstat` after `mmap`, cap the
  mapped length, fail if the file shrank. Cheap, no new machinery, but only
  narrows the `SIGBUS` window — a truncation landing between the last check
  and an access still kills the process.
- **B — Install a `SIGBUS` handler.** Closes the window, but a
  process-global signal handler in a *library* is a real imposition on the
  host application (interacts with other handlers, needs `siglongjmp` out
  of the handler, thread-safety, save/restore of any prior handler).
- **C — Drop `mmap` for `pread`/`pwrite`.** No mapping, no `SIGBUS`: a read
  past EOF becomes an ordinary short read / `io::Error`. Diverges from how
  SQLite itself accesses the wal-index via `MAP_SHARED`, so the
  reader-mark protocol's cross-process visibility guarantees needed
  re-validating rather than assumed.

## Decision

**Option C.** It is the only option that eliminates the failure mode
instead of shrinking its window, and "a library can kill the host process"
outweighs "diverges from SQLite's own implementation strategy."

Cross-process visibility was the open question blocking C: `MAP_SHARED`
stores are what make a published `aReadMark` visible to a live checkpointer
in SQLite's own implementation, and spike 005 (experiment 4,
`sqlite-rs:tests/spike/005_locking_interop/findings.md`) validated the reader-mark
protocol specifically against that mmap shape. Buffered `pread`/`pwrite`
instead of `mmap` relies on a different mechanism for the same guarantee:
the OS's unified page cache keeps buffered file I/O and `mmap`'d access to
the same file coherent. This holds on Linux and macOS — sqlite-rs's
supported platforms — so a stock `sqlite3` checkpointer's `mmap`'d view and
sqlite-rs's `pread`/`pwrite`'d view of the same `-shm` file stay coherent
without re-running the spike's live-interop experiment.

Option B was not pursued — C fully closes the exposure without a
process-global signal handler's maintenance cost, so B's added risk bought
nothing further.

## Consequences

- `src/storage/row/vfs/shm.rs` has no `mmap`, and no `unsafe` — the crate-wide
  `forbid(unsafe_code)` is restored (#66).
- A truncated `-shm` file (shrunk out from under a reader, or a
  crash-truncated / half-written file) yields a structured `io::Error`
  from the failing `pread`/`pwrite`, never a signal.
- The filesystem-reported `-shm` length is still validated before any
  offset into it is trusted: `validate_shm_len` rejects both a file too
  short for a full wal-index header (`MIN_SHM_LEN`) and, per #54's
  remaining scope, one larger than any size a cooperating writer produces
  (`MAX_SHM_LEN`, 8 of SQLite's 32KB `-shm` regions). There is no `usize`
  narrowing of the length to guard here — `pread`/`pwrite` offsets are
  `u64`, not a pointer-sized mapped-length argument, so the >4GB
  32-bit-target concern from #54's acceptance criteria does not apply to
  this access strategy.
