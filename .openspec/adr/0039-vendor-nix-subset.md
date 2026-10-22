# ADR 0039: Vendor a `nix` subset: reintroduce a single, narrow `unsafe` boundary

> Moved from t-rust-db/tools `sqlite-rs/.openspec/adr/0031-vendor-nix-subset.md` (#575), keeping its `fcntl` half. The `termios` half went to db-cli with the line editor (t-rust-db/tools `db-cli/src/editor.rs`). Paths are db-core's.

**Status:** Accepted · **Date:** 2026-08-26 (partially supersedes sqlite-rs ADR-0009)

## Context

sqlite-rs ADR-0009 eliminated the crate's `unsafe` entirely (#66) by moving the
remaining raw `fcntl`/`mmap`/`fork` calls onto `nix`'s safe wrappers and
`std`, and declared `#![forbid(unsafe_code)]` crate-wide — "zero unsafe in
this codebase" as a machine-checked (`check-mvl-limit`, `cargo geiger`) claim.

That claim was always narrower than "zero external trust": `nix` still
carries the actual `unsafe extern "C"` FFI, plus its own transitive deps
(`libc`, `cfg-if`, `bitflags`) and a `build.rs`. For a forensics/FRANK
positioning where the external-dependency supply chain (typosquatting, a
compromised maintainer, a malicious update) is the threat model that
matters most, "zero unsafe in our own source" and "zero external trust"
are different guarantees, and #563 asked for the second one.

## Decision

Vendor the `unsafe extern "C"` FFI that `nix::fcntl` provided (byte-range
`fcntl(F_SETLK/F_GETLK/F_SETLKW)` locking) into
`src/storage/row/vfs/fcntl.rs`, hand-writing the platform ABI (`struct
flock` and its constants) for macOS and Linux from each platform's own
headers rather than depending on the `libc` crate for it. No `nix`
dependency remains.

This is one audited `unsafe` boundary: `unsafe_code = "deny"` crate-wide,
with a scoped `#[allow(unsafe_code)]` in `fcntl.rs` only, and `fcntl.rs`
listed in `make check-mvl-limit`'s exclusions. Every other module —
`src/storage/row/vfs/lock.rs`, `src/storage/row/vfs/shm.rs` and
`src/storage/row/vfs/test_lock_probe.rs` included, sqlite-rs ADR-0009's
actual subject — stays unsafe-free: they call `fcntl.rs`'s *safe* wrapper
functions, the same call shape as the `nix` APIs they replace. ADR 0016
lists this carve-out alongside `storage::column::mmap`, the crate's only
other one.

## Alternatives rejected

- **Depend on the `libc` crate instead of vendoring.** Removes `nix`'s
  safe-wrapper layer but keeps an external dependency (and `libc`'s own
  supply chain) for the ABI structs/constants this ADR hand-writes
  instead — doesn't reach #563's "zero external dependencies" goal.
- **Forbid `unsafe` and find a pure-Rust alternative.** No pure-Rust or
  `std` API exists for POSIX byte-range record locks: `std::fs`'s locking
  stops short of what SQLite's locking protocol needs. The FFI boundary is
  unavoidable, not merely `nix`'s implementation choice.

## Consequences

- No `nix`, and no `libc` crate pulled in through it, for row storage: the
  only trust boundary left is the Rust compiler and the two platforms' own
  libc/kernel ABIs, which every Rust binary already links against.
- sqlite-rs ADR-0009's "zero unsafe, no override possible anywhere" claim
  is narrowed to "zero unsafe outside one audited module" — a deliberate
  loosening, not a regression on sqlite-rs ADR-0009's motivation (getting `unsafe`
  out of the VFS's locking/mmap logic, which stands unchanged).
- `fcntl.rs`'s FFI is exercised by SQLite's real locking test suite
  (`src/storage/row/vfs/lock.rs`, `src/storage/row/vfs/shm.rs`'s
  subprocess-contention tests) plus direct unit tests in
  `src/storage/row/vfs/fcntl.rs` — the ABI structs are load-bearing for
  every one of those, so a layout mistake fails loudly rather than
  silently.
