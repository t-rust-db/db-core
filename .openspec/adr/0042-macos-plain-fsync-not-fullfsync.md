# ADR 0042: On macOS, `Vfs::sync` calls plain `fsync(2)`, not `std`'s `F_FULLFSYNC`

> Moved from t-rust-db/tools `sqlite-rs/.openspec/adr/0037-macos-plain-fsync-not-fullfsync.md` (#575). Paths are db-core's; `sqlite-rs:` paths are in t-rust-db/tools' `sqlite-rs/`.

Date: 2026-08-30

## Context

#652: single-row write benchmarks (`insert_single`, `update_pk`,
`delete_pk`, ...) were 5-11x slower than the pinned oracle even after
#648 fixed the index-decode overhead #648 targeted. Flamegraph profiling
pointed at "file-lifecycle syscalls" (`open`/`close`/`fcntl`/`pread`/
`pwrite`), but a code audit showed the journal lifecycle was already
minimal (single open, single unlink, no redundant opens) and the
journal-mode lock ladder byte-identical to stock SQLite's `os_unix.c`
(spike 005) — nothing there was avoidable overhead.

Temporary `Instant`-based wall-clock instrumentation (an on-CPU
flamegraph can't see time blocked in a syscall waiting on the disk, so
this needed a different technique) around every step of
`Pager::flush_locked` isolated the real cost: the two required
`fsync`/`fdatasync` calls (`JournalWriter::sync`, `Pager::flush_locked`'s
main-file sync) alone accounted for ~65-75% of `insert_single`'s total
wall time (10-13ms of 15-19ms).

A follow-up probe compared four ways of forcing a durable write on this
crate's own dev hardware (a real disk, not tmpfs):

| call                          | avg latency |
|-------------------------------|------------:|
| raw `fsync(2)`                | ~0.05 ms    |
| `std::fs::File::sync_data()`  | ~4.1 ms     |
| `std::fs::File::sync_all()`   | ~4.2 ms     |
| explicit `fcntl(F_FULLFSYNC)` | ~4.4 ms     |

`std`'s two sync methods match `F_FULLFSYNC`'s cost almost exactly, not
plain `fsync`'s — confirming that on Apple platforms, Rust's standard
library upgrades both `sync_data` and `sync_all` to `fcntl(F_FULLFSYNC)`
(a full flush past the drive's write cache), not the POSIX `fsync(2)`
call their names suggest.

Checking the pinned oracle directly settles what its own default is:

```
$ sqlite3 :memory: "PRAGMA fullfsync; PRAGMA checkpoint_fullfsync;"
0
0
```

`PRAGMA fullfsync` — SQLite's own macOS-specific knob for exactly this
upgrade — defaults to **off**. Stock SQLite's `synchronous=FULL` (also
the oracle's own default, confirmed via `PRAGMA synchronous` → `2`) calls
plain `fsync()` on macOS unless a connection opts into `fullfsync`
itself. This crate's `Vfs::sync` — by using `std::fs::File::sync_data`
unconditionally — was silently paying `F_FULLFSYNC`'s cost on every
single commit, on every platform, regardless of any `PRAGMA fullfsync`
equivalent (this crate has none), which is *stronger* durability than
the oracle's own default and explains the bulk of the 5-11x gap #652
set out to close.

## Decision

On macOS only, `UnixVfsFile::sync` calls a vendored plain `fsync(2)`
(`crate::sys::fcntl::fsync`, alongside the existing vendored
`F_SETLK`/`F_GETLK` FFI — see ADR-0039) instead of
`std::fs::File::sync_data`/`sync_all`. Linux is unaffected: `std`'s
`sync_data` already calls plain `fdatasync` there (verified: no
Linux-specific `F_FULLFSYNC`-equivalent upgrade exists in `std`), which
already matches SQLite's own Linux default.

This makes `synchronous=FULL`'s actual on-disk behavior match the
oracle's own default byte-for-byte-equivalent durability contract on
macOS: a plain `fsync()`, not a full media flush. `PRAGMA fullfsync`
itself is not implemented (no code path needs stronger-than-`fsync`
durability yet) — this ADR only removes an *unintentional*, undocumented
upgrade this crate was never asked to make.

## Alternatives rejected

- **Implement `PRAGMA fullfsync` and default it off**, rather than just
  swapping the underlying syscall. Rejected as premature: nothing in
  this crate's PRAGMA surface currently distinguishes "oracle-default
  durability" from "maximum durability" for anything else (there's no
  equivalent knob for, say, WAL checkpoint fsync strength either), and
  #652's acceptance criteria only asks for behavior/performance parity
  with the oracle's default, not a new user-facing knob. A future ticket
  can add `PRAGMA fullfsync` on top of this plain-`fsync` default exactly
  the way stock SQLite layers it, without revisiting this decision.
- **Keep `std::fs::File::sync_data` and instead reduce fsync *count***
  (e.g. WAL-only, or skip the main-file fsync under `FULL`). Rejected:
  changes durability semantics `PRAGMA synchronous`/`journal_mode`
  already exist to control (ADR-0041), where #652's own acceptance
  criteria explicitly scopes that tradeoff to the user, not a silent
  default change. The plain-`fsync` fix instead makes the *existing*
  default match the oracle's, with no semantics change at all.
- **Depend on the `libc` crate for `fsync`/`F_FULLFSYNC` constants**
  instead of vendoring one more `extern "C"` declaration into
  `src/storage/row/vfs/fcntl.rs`. Rejected for the same reason ADR-0039 vendors
  `fcntl`/`termios` rather than depending on `libc`: `src/storage/row/vfs/` is
  already the crate's one FFI boundary and zero-external-dependency
  policy, so folding `fsync` in there is one more line inside an
  existing exemption, not a new one.

## Consequences

- `crate::sys::fcntl::fsync` (macOS-only) and `UnixVfsFile::sync`'s
  `cfg(target_os = "macos")`/`cfg(not(target_os = "macos"))` split are
  the only production-code changes; no `Pager`/VDBE-level behavior,
  on-disk format, or public API changes.
- Benchmarked win (`make -C sqlite-rs's former tests/performance crud`, this crate's own dev
  hardware): `insert_single` ~15ms → ~1.2ms (~93% reduction, now within
  ~1x of the oracle's own ~1.5ms); every other single-row write scenario
  (`update_pk`, `update_indexed_column`, `update_multi_column`,
  `delete_pk`, `delete_equality_bucket`) improved 80-91%; #648's range-
  operation wins (`update_filtered_range`, `delete_filtered_range`) are
  unaffected (not regressed; `update_filtered_range` improved slightly
  further).
- Durability on macOS is now *weaker* than before this change in the
  specific sense that a power-loss event during the drive controller's
  own write-cache window is no longer flushed through on every commit —
  but this exactly matches what the oracle's own default already
  accepts, so it is a parity fix, not a new risk this crate introduces
  relative to real SQLite.
- `check-mvl-limit`'s qualified subset already exempts `src/storage/row/vfs/`
  (ADR-0039); no gate changes needed. `src/storage/row/vfs/unix.rs` itself stays
  outside the `unsafe` boundary — it calls the vendored safe wrapper,
  same shape as its existing `fcntl_call` usage.
