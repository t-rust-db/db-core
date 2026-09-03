# ADR 0001: Layered, synergetic architecture across db-core

> Source: session discussion, 2026-09-02/03 — restructuring column-rs onto
> `db-core`, then working out where `RowExecutor`/`BatchExecutor`/
> `StreamExecutor` and error handling should converge as the layered,
> synergetic architecture matured

## Status

Accepted. Partially implemented: `sql-vm` (batch real, row/stream stubs)
and `sql-error` (`Span` only) exist; `sql-column`/`sql-codegen` do not
yet.

## Context

`db-core` started as an extraction target for column-rs's private
modules (`sql-types`, `sql-expr`, `sql-parser`, `sql-join`), with no
settled position on two open questions:

1. **Where do the query executors live?** The initial design (see
   `projects/database-rs/unified-vm-vision.md`, outside this repo) kept
   `RowExecutor` inside sqlite-rs, unshared — reasoned as "cursor/B-tree-
   coupled, nothing to share." `BatchExecutor`/`StreamExecutor` were
   slated for a shared `sql-vm-core` crate.
2. **What does error handling look like across crates?** No decision had
   been made; each crate so far (`sql-parser::ParseError`,
   `sql_vm::batch::VmError`) independently hand-rolled its own enum,
   by default rather than by policy.

sqlite-rs is the more mature codebase in this family — real, tested,
shipping, with its own settled conventions (`.openspec/adr/`, per-module
hand-rolled errors, zero external error-handling crates, `Span`-carrying
parse errors). The question this ADR settles: does `db-core` converge
toward sqlite-rs's conventions where one exists, or invent its own?

## Options considered

**On executor placement:**
- **A — Keep the original split.** `RowExecutor` stays inside sqlite-rs,
  only `BatchExecutor`/`StreamExecutor` shared. Minimal disruption, but
  means "the VM" is split across two repos by execution strategy, and a
  future sqlite-rs integration has to reach across repos for anything
  touching row execution.
- **B — Consolidate all three executors into one `db-core` crate**
  (`sql-vm`), even though only `batch` has real content yet. `row`/
  `stream` become documented stubs rather than a decision deferred
  silently.

**On error handling:**
- **A — A central `Error` enum in `sql-error`,** unifying every crate's
  failure mode into one type (the shape a `thiserror`-based design
  typically produces).
- **B — Mirror sqlite-rs exactly:** no central `Error` type, no external
  error crates, each crate keeps its own hand-rolled enum, composed by
  wrapping (`PagerError::Wal { path, source: WalError }`-style) rather
  than flattened. `sql-error` holds only a genuinely cross-cutting
  primitive (`Span`, for location-carrying parse errors) that sqlite-rs
  already has and `db-core`'s own parser doesn't yet.

## Decision

**Executors: Option B.** `sql-vm` now holds `batch` (real, extracted from
column-rs's `vm.rs`), `row` and `stream` (stubs, each with a doc comment
stating explicitly that they are not a port of sqlite-rs's actual VDBE
and that whether `row` ends up sharing an opcode *set* with sqlite-rs or
just the row-at-a-time *execution strategy* is still open). Each executor
is gated behind its own Cargo feature (`batch`/`row`/`stream`), so a
consumer like column-rs depends on exactly the one it uses
(`default-features = false, features = ["batch"]`) without compiling the
others or their dependencies.

This supersedes the original "keep `RowExecutor` in sqlite-rs" call —
not a walk-back of a mistake, but the layered/synergetic architecture
this ADR is named for reaching its natural endpoint once the executor
question was actually worked through, rather than settled prematurely
on the first plausible split. Recorded honestly in `t-rust-db/grammar/
ALIGNMENT.md` §3 as a superseded decision, not silently overwritten.
**Nothing has moved out of sqlite-rs itself.** sqlite-rs's own VDBE
(`src/vdbe/program.rs`, ~65 opcodes) is untouched; `sql-vm::row` is
reserved space, not a migration target, until a real decision is made
about whether/how sqlite-rs adopts it.

**A driving principle throughout: column-rs stays as thin as possible.**
Every piece that isn't genuinely Parquet-specific belongs in a shared
`db-core` crate, not in column-rs itself — `sql-vm` (this ADR) is the
first big move in that direction, and the anticipated `sql-column`
extraction (see Consequences) is the next one. column-rs's own repo
should shrink toward "Parquet I/O glue + wiring," not grow independent
logic other engines (loglume, eventually sqlite-rs where applicable)
would also need.

**Error handling: Option B.** `sql-error` holds `Span` only — field-for-
field identical to sqlite-rs's own (`line`, `column`, `offset`, `len`),
because sqlite-rs already solved this problem and there is no reason to
re-derive a different shape. No central `Error` type. No `thiserror`/
`anyhow`/any external error crate, anywhere in `db-core` — matching
sqlite-rs's own `Cargo.toml`, which has none either. Each crate's error
enum stays independent, composing lower-layer errors as wrapping variants
when it needs to, exactly as sqlite-rs's `PagerError`/`BtreeError`/
`RecordError` already do.

## Consequences

- `db-core` crates converge toward sqlite-rs's conventions by default,
  not by coincidence — a future reviewer familiar with sqlite-rs should
  find `db-core`'s shape unsurprising.
- Two genuinely different opcode sets (`sql_vm::batch::Opcode`, a future
  `sql_vm::row::Opcode`) living in one crate is an intentional trade:
  consolidated *location*, not a shared *representation*. Nothing in this
  ADR claims they should become one opcode enum.
- `sql-parser::ParseError` migrating to carry `Span` (tracked alongside
  this ADR, not yet landed at time of writing) is the first real proof
  this convergence pays for itself, not just a symbolic gesture.
- Follow-up architectural layering already anticipated but not yet
  built, for the same reason (sqlite-rs's codegen has no equivalent,
  since VDBE has no per-segment-parallelism to merge): a `sql-column`
  crate (the columnar query planner — `compile()`/`Plan`/`AggPart`/
  `output_column_names`, currently private to column-rs's `query.rs`)
  shared between column-rs's interpreter and a future `sql-codegen`
  crate, and new `sql_vm::batch` opcodes (`HashBuild`/`HashProbe`/
  `Window`, tracked as `db-core#2`) to replace `execute_joined`/
  `execute_semi_join`/`execute_windowed`'s current VM-bypassing plain-Rust
  functions.
