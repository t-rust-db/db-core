# ADR 0000: Charter

## Status

Accepted (#324). Outside the numbered sequence by design: nothing precedes
it and nothing supersedes it. ADR 0001 onward must be consistent with it;
a conflict resolves toward this document.

## Purpose

db-core exists to be a **safe SQLite in Rust** that can be audited: a
Rust engine that reads and writes real SQLite files with sqlite3-oracle
parity, whose SQLite path has no third-party dependencies, confines
`unsafe` to a named list, is written in a qualified language subset, has
MC/DC on every multi-leaf decision, and defines "correct" as agreement
with the pinned sqlite3 on the oracle corpus and sqllogictest.

Other execution modes (column over Parquet, stream over logs) reuse the
verified core -- parser, value model, planner infrastructure, gates --
under the same bar. They are welcome as long as they cannot change what
the SQLite path compiles to.

## The SQLite profile

The exact feature set sqlite-rs builds db-core with:

```
parser-row, vm-row, codegen-row, storage-row, engine-row
```

Everything this profile compiles is on the audit path. The set is
measured, not declared: `tools/check_sqlite_profile.py sqlite` takes
rustc's dep-info for that build and checks the invariants below against
the files actually compiled.

## The stream profile

The log engine's execution mode (db-core#403):

```
parser-column, vm-batch, vm-stream, codegen-batch, codegen-stream, storage-stream, engine-stream
```

Invariants (a) and (b) apply here too -- "under the same bar" from the
Purpose section above, made mechanical -- via
`tools/check_sqlite_profile.py stream`. There is no mode-isolation
invariant for this profile: it legitimately compiles `vm::batch` and
`codegen::batch` (its planner reuses their types).

## The column profile

The Parquet analytics mode (db-core#405):

```
parser-column, storage-column, engine-column
```

Unlike the SQLite and stream profiles, this one is **not** first-party-only
by design: `storage-column = ["dep:memmap2", "dep:ruzstd"]` are the crate's
only third-party dependencies, accepted because hand-rolling a
cross-platform mmap wrapper or a from-scratch zstd decoder is not worth the
risk of getting page-fault/lifetime or decompression edge cases wrong for a
mode with no oracle-parity invariant (f) obligation. Invariant (a) (first-
party dependency closure) therefore does not apply to this profile; invariant
(b) does, with its own carve-out below. `tools/check_sqlite_profile.py
column` checks (b) only.

## Invariants and their gates

| # | Invariant | Gate | Runs |
|---|---|---|---|
| (a) | The SQLite and stream profiles' dependency closures are first-party only (`db-core`). | `make check-sqlite-profile`, `make check-stream-profile` (`cargo tree` over each profile) | every PR |
| (b) | `unsafe` on the SQLite profile is exactly the named carve-outs, count checked: `storage/row/vfs/fcntl.rs` -- 2 (`fsync`, `fcntl` byte-range locks; ADR-0031 lineage). The stream profile carries none. The column profile carries one: `storage/column/mmap.rs` -- 1 (`memmap2::Mmap::map`; db-core#405, Safety comment at the call site). | `make check-sqlite-profile`, `make check-stream-profile`, `make check-column-profile` (regex over each profile's dep-info file set) | every PR |
| (c) | The SQLite side (`parser/row`, `vm/row`, `codegen/row`, `storage/row`, `engine/row`) never names `vm::batch`, `vm::engine`, `vm::stream`, `codegen::batch`, `storage::column`, `storage::stream`, `engine::column`, `engine::stream`; and those never name the SQLite side. The SQLite profile compiles no file from another mode. | `tests/unit/layer_isolation_test.rs`; `make check-sqlite-profile` | every PR |
| (d) | Every Cargo feature builds standalone with its declared implications and nothing else. | `make check-features` | every PR |
| (e) | The MC/DC obligation snapshot is current and every tagged vector names a real obligation. | `make check-mcdc-fresh`; `tests/unit/mcdc_discharge_test.rs` | every PR |
| (f) | Oracle parity is the definition of correct: sqlite-rs's corpus and sqllogictest against the pinned sqlite3, on the pinned db-core. | sqlite-rs CI (`corpus`), sqlite-rs `assurance.yml` (weekly) | every sqlite-rs PR; weekly |
| (g) | Line coverage floor, measured on the SQLite profile, not on `--all-features`. | `make check-coverage-profile` in `assurance.yml` | weekly |

Shared leaves both sides may use: `value`, `schema`, `types`, `coerce`,
`compare`, `functions`, `parser` (the batch grammar is an adapter over the
row grammar), `vm::join`, and `engine.rs` (the seam, ADR 0017).

The fix for a violation of (c) is never a feature implication. Moving the
shared item to a leaf module is.

## Language rules

- No `unsafe` outside (b). A safe-to-call intrinsic is not a carve-out; an
  `unsafe` block is, and needs this document changed.
- Qualified subset (`make check-mvl-limit`): explicit lifetimes and `dyn`
  only at designated boundaries, named by file in `MVL_LIMIT_EXCLUDE`
  (ADR 0008: `vm/row` cursor traits; ADR 0016 §Gates: the VFS traits, the
  two `unsafe` carve-outs, the Parquet zero-copy reader).
- Production code returns typed errors and never panics
  (`check-panic-allows`, `EXEMPT` empty). Test code fails fast.
- Untrusted integers from a file go through `try_from`; a reinterpretation
  the file format itself defines (rowid varint bits, DELTA_BINARY_PACKED)
  is spelled out once, in a named helper.
- SIMD/NEON is welcome anywhere it measurably pays, the SQLite path
  included, under the rules above: safe intrinsics or autovectorized Rust
  only; any `cfg(target_arch)` code has a portable fallback and both paths
  are tested for identical results; if the qualified subset does not admit
  the construct, that is a decision recorded here, not a workaround.

## Non-goals

Async I/O, network access, loadable extensions, performance parity with
sqlite3 as a release criterion.

## Relation to the numbered ADRs

ADR 0001-0018 describe how; this document describes what for and what may
never change while doing it.
