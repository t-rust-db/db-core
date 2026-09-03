# ADR 0003: `db-storage::{Vfs, VfsFile}` vs sqlite-rs's `vfs::{Vfs, VfsFile}` — two traits, not one

> Source: `#14` — reconcile `db-storage` with sqlite-rs's `src/vfs` (Phase 1)

## Status

Accepted. `sqlite-rs`'s `src/vfs/*` extracted verbatim into a new
`db-core` crate, `sql-vfs`, on its own trait — `db-storage`'s
`{Vfs, VfsFile}` is untouched.

## Context

`db-storage` (a separate repo, `~/wc/t-rust-db/db-storage`) already has
a minimal `Vfs`/`VfsFile` pair (48 lines, `src/vfs.rs`): generic
associated type dispatch (`Vfs::File: VfsFile`, no `dyn`), read-only
(`size`, `read_at`, `mmap`), backed by a real `memmap2::Mmap` or an
`Arc<[u8]>`. Its one `unsafe` (`src/mmap.rs`, `memmap2::Mmap::map`) is
called out in the crate's own module doc as "the only place in this
crate that uses `unsafe`." Its only consumer, `column-rs`
(`src/query.rs::QueryEngine::add_table`), opens a Parquet file once,
`mmap()`s it whole, and hands the byte slice to `ParquetFile::open` —
no `read_at` calls, no writes, no concurrent-mutation exposure.

sqlite-rs's own `src/vfs/*` (3,150 lines: `vfs.rs`, `lock.rs`, `shm.rs`,
`unix.rs`, `memory.rs`, `page_source.rs`, `test_lock_probe.rs`) is a
`dyn`-boxed trait pair covering the full ACID file surface: read/write/
create/delete, a journal-mode SHARED/RESERVED/PENDING/EXCLUSIVE
`fcntl`-lock ladder (`lock.rs`), and WAL `-shm` reader-mark/checkpoint/
write-lock coordination via `pread`/`pwrite` (`shm.rs`) — deliberately
**not** `mmap`, per its own
[`ADR-0001`](https://github.com/iheitlager/sqlite-rs/blob/main/.openspec/adr/0001-shm-access-pread-not-mmap.md):
`mmap`-ing a `-shm` file that another process truncates concurrently
(`PRAGMA wal_checkpoint(TRUNCATE)`) raises `SIGBUS` — an uncatchable
process kill — so sqlite-rs moved off `mmap` entirely for anything with
concurrent-mutation exposure. Its
[`ADR-0009`](https://github.com/iheitlager/sqlite-rs/blob/main/.openspec/adr/0009-zero-unsafe-syscall-wrappers.md)
generalizes this crate-wide: `#![forbid(unsafe_code)]`, with the one
remaining FFI `unsafe` pushed down into `sql-sys`'s vendored syscall
shim (`ADR-0031`), not the VFS logic itself.

## Options considered

- **A — One unified trait**, sqlite-rs's shape wins where they
  conflict (per this session's driving principle: sqlite-rs as the
  mature/leading basis), `db-storage`'s minimal usage becomes a subset
  of it.
- **B — Two traits.** `sql-vfs` (this crate) carries sqlite-rs's full
  surface unchanged; `db-storage::{Vfs, VfsFile}` stays as-is,
  untouched, still serving column-rs's narrower need.

## Decision

**Option B.** The two designs aren't a "pick sqlite-rs's shape"
situation like `Value` in `t-rust-db/grammar/ALIGNMENT.md` — they
actively **disagree on a decision sqlite-rs already made and
justified**:

- `db-storage`'s entire reason to exist is `mmap()`-the-whole-file
  zero-copy reads. That's safe today only because column-rs's Parquet
  file is genuinely static once opened — no concurrent writer, no
  `-shm` involved. sqlite-rs's `ADR-0001` didn't reject `mmap` in
  general; it rejected `mmap` specifically for anything exposed to
  concurrent mutation, because the failure mode (`SIGBUS`) is a crash,
  not a recoverable error. A unified trait that keeps `mmap()` as a
  required member risks a writable/WAL-aware backend implementing it
  and reintroducing exactly the hazard `ADR-0001` eliminated; a unified
  trait that drops `mmap()` in favor of `read_at`-only removes
  column-rs's current zero-copy fast path for a consumer that has no
  locking/WAL need to justify losing it.
- `db-storage`'s one `unsafe` is a direct conflict with sqlite-rs's
  `ADR-0009` zero-unsafe stance the moment the two share a crate or
  trait hierarchy — `db-storage` would either have to adopt the same
  policy (dropping real `mmap` for `pread`, i.e. becoming Option A in
  disguise) or `sql-vfs` would have to carve out an `unsafe` exception
  it spent `ADR-0009` removing.
- The consumers' needs don't overlap enough to make unification pay
  for itself: column-rs never needs locking, WAL, writes, or deletes;
  sqlite-rs's pager always needs all of them. A unified trait's shared
  surface would be `size`/`read_at` only — everything else would be a
  per-backend optional method with a default no-op, which is most of
  what a "unified" trait would look like anyway, for no reduction in
  either side's actual code.

So: two traits, matching `#14`'s own escape hatch ("or two traits if
unification genuinely doesn't pay for itself"). `sqlite-rs`'s
`src/vfs/*` moves into a new `db-core` crate, `sql-vfs`, verbatim and
on its own trait — same extraction pattern as `sql-sys` (`#11`) and
`sql-record` (`#13`). `db-storage::{Vfs, VfsFile}` is not touched.

### `sql-vfs` extraction details

- Flat module layout (`src/{lib,lock,memory,page_source,shm,
  test_lock_probe,unix}.rs`), matching `sql-sys`'s convention, not the
  original `src/vfs/` subdirectory nesting.
- Depends on `sql-sys` for `fcntl` (byte-range locking FFI) — the one
  crate-internal dependency the original `crate::sys::fcntl` had,
  rewired to `sql_sys::fcntl` since `sql-sys`'s API is (still) a
  verbatim copy of what sqlite-rs's own `src/sys/fcntl.rs` exports.
  sqlite-rs's own `src/vfs/*` has **not** been switched to depend on
  `sql-sys` yet (`#11`'s note applies here too: "sqlite-rs's own
  src/sys/* is left untouched — wiring it to depend on sql-sys instead
  is a separate change in that repo").
- `#![deny(unsafe_code)]` at the crate root, matching sqlite-rs's own
  `src/lib.rs`; zero `unsafe` in the moved code, consistent with
  `ADR-0009`.
- The `lock_probe` test helper (`tests/helpers/lock_probe.rs` in
  sqlite-rs, a genuine second-process lock holder needed by
  `lock.rs`/`shm.rs`'s contention tests) moved to `src/bin/lock_probe.rs`
  so Cargo auto-discovers it as a sibling binary without a `[[bin]]`
  entry.
- Several `pub(crate)` items (`FileLock`'s `check_reserved`/
  `escalate_to_exclusive`/`de_escalate_to_shared`/`set_level`,
  `AnyWalShm`'s `claim_write_lock`/`release_write_lock`/
  `publish_mx_frame`, `lock::reserved_byte_range`,
  `shm::fresh_shm_bytes`) and the `lock`/`shm` modules themselves
  promoted from `pub(crate)` to `pub`. In sqlite-rs's own crate these
  were reachable from the sibling `pager` module; `pager` doesn't move
  in this issue, so without promotion `cargo clippy -D warnings` flags
  them all as dead code. Promoting them to `pub` is the correct fix,
  not a workaround: `sql-vfs` is now a library awaiting an external
  consumer (`sqlite-rs`'s own `pager`, in a later phase), and `pub`
  items in a lib crate are exempt from the `dead_code` lint precisely
  because reachability from outside the crate can't be proven locally.

## Consequences

- `db-storage` needs no change. Verified: column-rs's full test suite
  (35 tests, `~/wc/t-rust-db/column-rs`) passes unchanged.
- `sql-vfs`'s own test suite (49 tests, including the `lock_probe`
  cross-process contention tests) passes; `cargo clippy --all-targets
  -D warnings` and `cargo fmt --check` are clean.
- sqlite-rs's own `src/vfs/*` is **not** deleted or repointed at
  `sql-vfs` by this ADR — same deferral `#11` made for `sql-sys`. That
  rewiring (updating sqlite-rs's `Cargo.toml`/imports, deleting the
  now-duplicated `src/vfs/*`, running sqlite-rs's full test suite
  against the extracted crate) is a separate change in the sqlite-rs
  repo, tracked as a follow-up issue rather than folded into `#14`.
- If `db-storage` ever needs locking or WAL-awareness (no evidence of
  that today — column-rs's need is unchanged), re-diff against this
  ADR's reasoning before assuming `sql-vfs`'s trait is "the" shared
  one — the conflict recorded here (mmap-safety vs pread-only) would
  need revisiting, not just a trait-shape merge.
