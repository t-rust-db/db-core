# ADR 0003: Two VFS traits, not one

## Status

Accepted. The traits live in `db-storage` (ADR 0006); this ADR records
the boundary decision `db-core`'s executors are built against.

## Decision

Physical storage exposes **two separate `Vfs`/`VfsFile` trait pairs**,
one per storage mode, and they are not unified:

- `db_storage::row::vfs` -- the full ACID file surface: read, write,
  create, delete, a `SHARED`/`RESERVED`/`PENDING`/`EXCLUSIVE` byte-range
  lock ladder, and WAL `-shm` coordination via `pread`/`pwrite`. Never
  `mmap`: a memory-mapped file another process truncates raises `SIGBUS`,
  an uncatchable process kill, so anything exposed to concurrent
  mutation reads through explicit syscalls.
- `db_storage::column` -- a minimal read-only pair (`size`, `read_at`,
  `mmap`) whose reason to exist is zero-copy `mmap()` of a whole file
  that is static once opened (a Parquet file). Its one `unsafe` is the
  map call, documented as such.

## Rationale

The two designs disagree on a safety decision, not just on surface
area. A unified trait that keeps `mmap` as a member invites a writable,
WAL-aware backend to reintroduce the `SIGBUS` hazard; one that drops it
removes the columnar reader's fast path for a consumer that has no
locking or WAL need to justify the loss. The genuinely shared surface
would be `size`/`read_at`, with everything else an optional method
defaulting to a no-op -- no reduction in either side's code.

## Consequences

- `vm::row` drives storage through its own cursor trait (ADR 0008), never
  through a VFS type; `vm::batch` receives segments from its embedding
  application. Neither executor names a VFS trait.
- If the columnar side ever needs locking or WAL awareness, revisit this
  reasoning (mmap safety versus pread-only) rather than merging trait
  shapes.
